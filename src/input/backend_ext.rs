use ::input as libinput;
use smithay::backend::input;
use smithay::output::Output;

use crate::protocols::virtual_pointer::VirtualPointer;
use crate::synoik::State;

pub trait SynoikInputBackend: input::InputBackend<Device = Self::SynoikDevice> {
    type SynoikDevice: SynoikInputDevice;
}
impl<T: input::InputBackend> SynoikInputBackend for T
where
    Self::Device: SynoikInputDevice,
{
    type SynoikDevice = Self::Device;
}

pub trait SynoikInputDevice: input::Device {
    // FIXME: this should maybe be per-event, not per-device,
    // but it's not clear that this matters in practice?
    // it might be more obvious once we implement it for libinput
    fn output(&self, state: &State) -> Option<Output>;
}

impl SynoikInputDevice for libinput::Device {
    fn output(&self, _state: &State) -> Option<Output> {
        // FIXME: Allow specifying the output per-device?
        None
    }
}

impl SynoikInputDevice for VirtualPointer {
    fn output(&self, _: &State) -> Option<Output> {
        self.output().cloned()
    }
}
