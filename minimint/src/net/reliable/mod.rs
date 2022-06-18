use serde::{Deserialize, Serialize};

pub mod queue;
pub mod session;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Ord, PartialOrd)]
pub struct MessageId(u64);

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Ord, PartialOrd)]
pub struct UniqueMessage<M> {
    id: MessageId,
    msg: M,
}

impl MessageId {
    pub fn increment(self) -> MessageId {
        MessageId(self.0 + 1)
    }
}
