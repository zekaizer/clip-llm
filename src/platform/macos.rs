use super::Platform;

pub struct MacOsPlatform;

impl Platform for MacOsPlatform {
    fn simulate_copy(&self) -> Result<(), crate::PlatformError> {
        todo!()
    }

    fn check_accessibility(&self) -> Result<(), crate::PlatformError> {
        todo!()
    }
}
