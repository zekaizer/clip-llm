use super::Platform;

pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn simulate_copy(&self) -> Result<(), crate::PlatformError> {
        todo!()
    }

    fn check_accessibility(&self) -> Result<(), crate::PlatformError> {
        // No-op on Windows
        Ok(())
    }
}
