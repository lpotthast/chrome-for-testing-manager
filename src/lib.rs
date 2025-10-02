mod cache;
pub mod chromedriver;
mod download;
pub mod mgr;
pub mod port;
pub mod session;

pub mod prelude {
    pub use crate::chromedriver::Chromedriver;
    pub use crate::mgr::ChromeForTestingManager;
    pub use crate::mgr::VersionRequest;
    pub use crate::port::Port;
    pub use crate::port::PortRequest;
    pub use crate::session::Session;
    pub use chrome_for_testing::api::channel::Channel;
    pub use chrome_for_testing::api::version::Version;
}
