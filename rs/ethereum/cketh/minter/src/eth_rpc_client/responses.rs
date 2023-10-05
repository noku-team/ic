use crate::eth_rpc::{Hash, HttpResponsePayload, Quantity, ResponseTransform};
use crate::numeric::{BlockNumber, Wei};
use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Encode, Decode)]
#[serde(rename_all = "camelCase")]
pub struct TransactionReceipt {
    /// The hash of the block containing the transaction.
    #[n(0)]
    pub block_hash: Hash,

    /// The number of the block containing the transaction.
    #[n(1)]
    pub block_number: BlockNumber,

    /// The total base charge plus tip paid for each unit of gas
    #[n(2)]
    pub effective_gas_price: Wei,

    /// The amount of gas used by this specific transaction alone
    #[cbor(n(3), with = "crate::cbor::u256")]
    pub gas_used: Quantity,

    /// Status of the transaction.
    #[n(4)]
    pub status: TransactionStatus,

    /// The hash of the transaction
    #[n(5)]
    pub transaction_hash: Hash,
}

impl HttpResponsePayload for TransactionReceipt {
    fn response_transform() -> Option<ResponseTransform> {
        Some(ResponseTransform::TransactionReceipt)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Encode, Decode)]
#[serde(try_from = "ethnum::u256")]
pub enum TransactionStatus {
    /// Transaction was mined and executed successfully.
    #[n(0)]
    Success,

    /// Transaction was mined but execution failed (e.g., out-of-gas error).
    /// The amount of the transaction is returned to the sender but gas is consumed.
    /// Note that this is different from a transaction that is not mined at all: a failed transaction
    /// is part of the blockchain and the next transaction from the same sender should have an incremented
    /// transaction nonce.
    #[n(1)]
    Failure,
}

impl TryFrom<ethnum::u256> for TransactionStatus {
    type Error = String;

    fn try_from(value: ethnum::u256) -> Result<Self, Self::Error> {
        match value {
            ethnum::u256::ZERO => Ok(TransactionStatus::Failure),
            ethnum::u256::ONE => Ok(TransactionStatus::Success),
            _ => Err(format!("invalid transaction status: {}", value)),
        }
    }
}

impl Display for TransactionStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TransactionStatus::Success => write!(f, "Success"),
            TransactionStatus::Failure => write!(f, "Failure"),
        }
    }
}
