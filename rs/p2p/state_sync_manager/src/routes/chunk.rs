use std::sync::Arc;

use crate::metrics::StateSyncManagerHandlerMetrics;
use crate::ongoing::DownloadChunkError;
use axum::{
    body::Bytes,
    extract::State,
    http::{Request, Response, StatusCode},
};
use bytes::BytesMut;
use ic_interfaces::state_sync_client::StateSyncClient;
use ic_logger::ReplicaLogger;
use ic_protobuf::{p2p::v1 as pb, proxy::ProxyDecodeError};
use ic_types::{
    artifact::StateSyncArtifactId,
    chunkable::{ArtifactChunk, ChunkId},
    NodeId,
};
use prost::Message;

pub const STATE_SYNC_CHUNK_PATH: &str = "/chunk";

pub(crate) struct StateSyncChunkHandler {
    _log: ReplicaLogger,
    state_sync: Arc<dyn StateSyncClient>,
    metrics: StateSyncManagerHandlerMetrics,
}

impl StateSyncChunkHandler {
    pub fn new(
        log: ReplicaLogger,
        state_sync: Arc<dyn StateSyncClient>,
        metrics: StateSyncManagerHandlerMetrics,
    ) -> Self {
        Self {
            _log: log,
            state_sync,
            metrics,
        }
    }
}

pub(crate) async fn state_sync_chunk_handler(
    State(state): State<Arc<StateSyncChunkHandler>>,
    payload: Bytes,
) -> Result<Bytes, StatusCode> {
    let _timer = state
        .metrics
        .request_duration
        .with_label_values(&["chunk"])
        .start_timer();

    let payload = pb::GossipChunkRequest::decode(payload).map_err(|_| StatusCode::BAD_REQUEST)?;

    let id: StateSyncArtifactId =
        bincode::deserialize(&payload.artifact_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let chunk_id = ChunkId::new(payload.chunk_id);

    // TODO: (NET-1442) move this to threadpool
    let jh = tokio::task::spawn_blocking(move || {
        state
            .state_sync
            .chunk(&id, chunk_id)
            .ok_or(StatusCode::NO_CONTENT)
    });
    let chunk = jh.await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;

    let pb_chunk: pb::ArtifactChunk = chunk.into();
    let mut raw = BytesMut::with_capacity(pb_chunk.encoded_len());
    pb_chunk.encode(&mut raw).expect("Allocated enough memory");

    Ok(raw.into())
}

pub(crate) fn build_chunk_handler_request(
    artifact_id: StateSyncArtifactId,
    chunk_id: ChunkId,
) -> Request<Bytes> {
    let pb = pb::GossipChunkRequest {
        artifact_id: bincode::serialize(&artifact_id).unwrap(),
        chunk_id: chunk_id.get(),
        integrity_hash: vec![],
    };

    let mut raw = BytesMut::with_capacity(pb.encoded_len());
    pb.encode(&mut raw).expect("Allocated enough memory");

    Request::builder()
        .uri(STATE_SYNC_CHUNK_PATH)
        .body(raw.freeze())
        .expect("Building from typed values")
}

/// Transforms the http response received into typed responses expected from this handler.
pub(crate) fn parse_chunk_handler_response(
    response: Response<Bytes>,
    chunk_id: ChunkId,
) -> Result<ArtifactChunk, DownloadChunkError> {
    let (parts, body) = response.into_parts();

    let peer_id = *parts
        .extensions
        .get::<NodeId>()
        .expect("Transport attaches peer id");
    match parts.status {
        StatusCode::OK => {
            let proto =
                pb::ArtifactChunk::decode(body).map_err(|e| DownloadChunkError::RequestError {
                    peer_id,
                    chunk_id,
                    err: e.to_string(),
                })?;
            let mut chunk: ArtifactChunk = proto.try_into().map_err(|e: ProxyDecodeError| {
                DownloadChunkError::RequestError {
                    peer_id,
                    chunk_id,
                    err: e.to_string(),
                }
            })?;
            // The TryFrom implementation always sets the chunk_id to zero.
            // Fix this by adding the correct chunk id.
            chunk.chunk_id = chunk_id;
            Ok(chunk)
        }
        StatusCode::NO_CONTENT => Err(DownloadChunkError::NoContent { peer_id }),
        StatusCode::TOO_MANY_REQUESTS => Err(DownloadChunkError::Overloaded),
        StatusCode::REQUEST_TIMEOUT => Err(DownloadChunkError::Overloaded),
        _ => Err(DownloadChunkError::RequestError {
            peer_id,
            chunk_id,
            err: String::from_utf8_lossy(&body).to_string(),
        }),
    }
}