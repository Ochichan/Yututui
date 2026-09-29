use std::time::Duration;

pub(crate) const OVERLAP_PROOF_TIMEOUT: Duration = Duration::from_secs(8);

pub(crate) const FADE_TICK: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExtraProof {
    Ready { epoch: u64 },
    Failed { epoch: u64 },
    TransportClosed { epoch: u64, from_extra: bool },
    DeckClosed { from_extra: bool },
}

impl ExtraProof {
    pub(crate) const fn epoch(&self) -> Option<u64> {
        match self {
            Self::Ready { epoch }
            | Self::Failed { epoch }
            | Self::TransportClosed { epoch, .. } => Some(*epoch),
            Self::DeckClosed { .. } => None,
        }
    }
}
