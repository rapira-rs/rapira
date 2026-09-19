mod message;
mod metadata;
mod types;
mod values;
pub(crate) use types::Job;
pub use types::{ErrorDetail, MethodInfo, Reply, Request, ServiceInfo, Status};
mod call;
mod view;
pub(crate) use call::{install, installed};

pub(crate) fn reclaim() {
    message::reclaim();
    call::reclaim();
}
