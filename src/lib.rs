use parking_lot::ReentrantMutex;

pub mod lwip;
pub mod stack;
pub mod tcp_listener;
pub mod tcp_stream;
pub mod udp;
pub mod util;

pub(crate) static LWIP_MUTEX: ReentrantMutex<()> = ReentrantMutex::new(());
