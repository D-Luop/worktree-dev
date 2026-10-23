//! Platform-free core of WorkTreeDev v2: the status state machine, the fleet model, and the
//! daemon wire protocol. No OS calls here, so a Linux build can reuse it unchanged.

pub mod model;
pub mod protocol;
pub mod status;
