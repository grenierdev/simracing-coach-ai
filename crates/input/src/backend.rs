//! Input sources: each one adds the inputs currently held.

use std::collections::HashSet;

use crate::Binding;

pub trait Backend: Send {
    /// Adds to `out` every input held right now.
    fn poll(&mut self, out: &mut HashSet<Binding>);
    /// Known devices (for display).
    fn devices(&self) -> Vec<String> {
        vec![]
    }
}

/// Keyboard and mouse, global state.
pub struct KeyboardMouse {
    state: device_query::DeviceState,
}

// SAFETY: DeviceState is only used from the input thread.
unsafe impl Send for KeyboardMouse {}

impl KeyboardMouse {
    pub fn new() -> Option<Self> {
        #[cfg(target_os = "linux")]
        let state = device_query::DeviceState::checked_new()?;
        #[cfg(not(target_os = "linux"))]
        let state = device_query::DeviceState::new();
        Some(KeyboardMouse { state })
    }
}

impl Backend for KeyboardMouse {
    fn poll(&mut self, out: &mut HashSet<Binding>) {
        use device_query::DeviceQuery;
        for k in self.state.get_keys() {
            out.insert(Binding::Key { key: k.to_string() });
        }
        let mouse = self.state.get_mouse();
        for (i, pressed) in mouse.button_pressed.iter().enumerate().skip(1) {
            if *pressed {
                out.insert(Binding::Mouse { button: i as u8 });
            }
        }
    }
}

/// Sources available on the platform.
pub fn platform_backends() -> Vec<Box<dyn Backend>> {
    let mut v: Vec<Box<dyn Backend>> = vec![];
    match KeyboardMouse::new() {
        Some(km) => v.push(Box::new(km)),
        None => tracing::warn!("keyboard and mouse unavailable (no display?)"),
    }
    #[cfg(windows)]
    match dinput::DirectInput::new() {
        Ok(di) => v.push(Box::new(di)),
        Err(e) => tracing::warn!("DirectInput unavailable: {e}"),
    }
    #[cfg(not(windows))]
    match gilrs::Gilrs::new() {
        Ok(g) => v.push(Box::new(Gamepads { gilrs: g })),
        Err(e) => tracing::warn!("gamepads unavailable: {e}"),
    }
    v
}

/// Gamepads and wheels on Linux (evdev, works in the background).
#[cfg(not(windows))]
pub struct Gamepads {
    gilrs: gilrs::Gilrs,
}

// SAFETY: Gilrs is only used from the input thread.
#[cfg(not(windows))]
unsafe impl Send for Gamepads {}

#[cfg(not(windows))]
impl Backend for Gamepads {
    fn poll(&mut self, out: &mut HashSet<Binding>) {
        while self.gilrs.next_event().is_some() {}
        for (_, gp) in self.gilrs.gamepads() {
            let device = gp.name().to_string();
            let instance: String = gp.uuid().iter().map(|b| format!("{b:02x}")).collect();
            for (code, data) in gp.state().buttons() {
                if data.is_pressed() {
                    out.insert(Binding::Controller {
                        device: device.clone(),
                        instance: instance.clone(),
                        button: code.into_u32() as u16,
                    });
                }
            }
        }
    }

    fn devices(&self) -> Vec<String> {
        self.gilrs
            .gamepads()
            .map(|(_, g)| g.name().to_string())
            .collect()
    }
}

/// Hat (POV) directions from its angle in hundredths of a degree.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn pov_directions(value: u32) -> Vec<u8> {
    if value & 0xFFFF == 0xFFFF {
        return vec![];
    }
    let a = value % 36_000;
    let mut d = vec![];
    if !(4_500..=31_500).contains(&a) || a == 4_500 || a == 31_500 {
        d.push(0);
    }
    if (4_500..=13_500).contains(&a) {
        d.push(1);
    }
    if (13_500..=22_500).contains(&a) {
        d.push(2);
    }
    if (22_500..=31_500).contains(&a) {
        d.push(3);
    }
    d
}

#[cfg(windows)]
mod dinput {
    //! DirectInput 8: game devices in the background, non-exclusive.
    //!
    //! Custom data format (the `windows` crate does not expose
    //! `c_dfDIJoystick2`): 128 buttons and 4 hats, all optional, to accept any
    //! wheel, pedal set or button box.

    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use windows::Win32::Devices::HumanInterfaceDevice::*;
    use windows::Win32::Foundation::{HINSTANCE, HWND};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, WINDOW_EX_STYLE, WS_OVERLAPPED,
    };
    use windows::core::{BOOL, Interface, w};

    use super::{Backend, pov_directions};
    use crate::Binding;

    const DIDFT_OPTIONAL: u32 = 0x8000_0000;
    const BUTTONS: usize = 128;
    const POVS: usize = 4;

    #[repr(C)]
    struct State {
        buttons: [u8; BUTTONS],
        povs: [u32; POVS],
    }

    struct Device {
        device: IDirectInputDevice8W,
        name: String,
        instance: String,
        failures: u32,
    }

    pub struct DirectInput {
        di: IDirectInput8W,
        hwnd: HWND,
        devices: Vec<Device>,
        last_enum: Instant,
        // Must stay valid as long as the devices are open.
        _objects: Box<[DIOBJECTDATAFORMAT; BUTTONS + POVS]>,
        format: Box<DIDATAFORMAT>,
    }

    // SAFETY: the DirectInput COM objects are only used from the input thread.
    unsafe impl Send for DirectInput {}

    fn wide(s: &[u16]) -> String {
        let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
        String::from_utf16_lossy(&s[..end])
    }

    unsafe extern "system" fn enum_cb(
        inst: *mut DIDEVICEINSTANCEW,
        ctx: *mut core::ffi::c_void,
    ) -> BOOL {
        unsafe {
            let list = &mut *(ctx as *mut Vec<DIDEVICEINSTANCEW>);
            list.push(*inst);
        }
        BOOL(DIENUM_CONTINUE as i32)
    }

    impl DirectInput {
        pub fn new() -> windows::core::Result<Self> {
            unsafe {
                let module = GetModuleHandleW(None)?;
                let hinst = HINSTANCE(module.0);
                let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
                DirectInput8Create(
                    hinst,
                    DIRECTINPUT_VERSION,
                    &IDirectInput8W::IID,
                    &mut raw,
                    None,
                )?;
                let di = IDirectInput8W::from_raw(raw);
                // Hidden window: DirectInput requires a top-level window, even in
                // background mode.
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("simcoach-input"),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(hinst),
                    None,
                )?;

                let mut objects: Box<[DIOBJECTDATAFORMAT; BUTTONS + POVS]> =
                    Box::new([DIOBJECTDATAFORMAT::default(); BUTTONS + POVS]);
                for (i, o) in objects.iter_mut().take(BUTTONS).enumerate() {
                    *o = DIOBJECTDATAFORMAT {
                        pguid: std::ptr::null(),
                        dwOfs: i as u32,
                        dwType: DIDFT_BUTTON | DIDFT_ANYINSTANCE | DIDFT_OPTIONAL,
                        dwFlags: 0,
                    };
                }
                for p in 0..POVS {
                    objects[BUTTONS + p] = DIOBJECTDATAFORMAT {
                        pguid: &GUID_POV,
                        dwOfs: (BUTTONS + p * 4) as u32,
                        dwType: DIDFT_POV | DIDFT_ANYINSTANCE | DIDFT_OPTIONAL,
                        dwFlags: 0,
                    };
                }
                let format = Box::new(DIDATAFORMAT {
                    dwSize: std::mem::size_of::<DIDATAFORMAT>() as u32,
                    dwObjSize: std::mem::size_of::<DIOBJECTDATAFORMAT>() as u32,
                    dwFlags: DIDF_ABSAXIS,
                    dwDataSize: std::mem::size_of::<State>() as u32,
                    dwNumObjs: (BUTTONS + POVS) as u32,
                    rgodf: objects.as_mut_ptr(),
                });
                let mut me = DirectInput {
                    di,
                    hwnd,
                    devices: vec![],
                    last_enum: Instant::now() - Duration::from_secs(60),
                    _objects: objects,
                    format,
                };
                me.enumerate();
                Ok(me)
            }
        }

        /// Opens newly plugged-in game devices.
        fn enumerate(&mut self) {
            self.last_enum = Instant::now();
            let mut list: Vec<DIDEVICEINSTANCEW> = vec![];
            unsafe {
                if let Err(e) = self.di.EnumDevices(
                    DI8DEVCLASS_GAMECTRL,
                    Some(enum_cb),
                    &mut list as *mut _ as *mut core::ffi::c_void,
                    DIEDFL_ATTACHEDONLY,
                ) {
                    tracing::warn!("DirectInput enumeration: {e}");
                    return;
                }
            }
            for inst in list {
                let instance = format!("{:?}", inst.guidInstance);
                if self.devices.iter().any(|d| d.instance == instance) {
                    continue;
                }
                let name = wide(&inst.tszProductName);
                match self.open(&inst) {
                    Ok(device) => {
                        tracing::info!("device: {name}");
                        self.devices.push(Device {
                            device,
                            name,
                            instance,
                            failures: 0,
                        });
                    }
                    Err(e) => tracing::warn!("opening {name}: {e}"),
                }
            }
        }

        fn open(&self, inst: &DIDEVICEINSTANCEW) -> windows::core::Result<IDirectInputDevice8W> {
            unsafe {
                let mut dev: Option<IDirectInputDevice8W> = None;
                self.di.CreateDevice(&inst.guidInstance, &mut dev, None)?;
                let dev = dev.ok_or_else(windows::core::Error::empty)?;
                dev.SetDataFormat(&*self.format as *const _ as *mut _)?;
                dev.SetCooperativeLevel(self.hwnd, DISCL_BACKGROUND | DISCL_NONEXCLUSIVE)?;
                let _ = dev.Acquire();
                Ok(dev)
            }
        }
    }

    impl Backend for DirectInput {
        fn poll(&mut self, out: &mut HashSet<Binding>) {
            if self.last_enum.elapsed() > Duration::from_secs(2) {
                self.enumerate();
            }
            for d in &mut self.devices {
                let mut st = State {
                    buttons: [0; BUTTONS],
                    povs: [0xFFFF_FFFF; POVS],
                };
                let ok = unsafe {
                    let _ = d.device.Poll();
                    d.device
                        .GetDeviceState(
                            std::mem::size_of::<State>() as u32,
                            &mut st as *mut _ as *mut core::ffi::c_void,
                        )
                        .is_ok()
                };
                if !ok {
                    d.failures += 1;
                    unsafe {
                        let _ = d.device.Acquire();
                    }
                    continue;
                }
                d.failures = 0;
                for (i, b) in st.buttons.iter().enumerate() {
                    if b & 0x80 != 0 {
                        out.insert(Binding::Controller {
                            device: d.name.clone(),
                            instance: d.instance.clone(),
                            button: i as u16,
                        });
                    }
                }
                for (h, v) in st.povs.iter().enumerate() {
                    for direction in pov_directions(*v) {
                        out.insert(Binding::Hat {
                            device: d.name.clone(),
                            instance: d.instance.clone(),
                            hat: h as u8,
                            direction,
                        });
                    }
                }
            }
            // Unplugged: remove it; enumeration will reopen it if it comes back.
            self.devices.retain(|d| d.failures < 200);
        }

        fn devices(&self) -> Vec<String> {
            self.devices.iter().map(|d| d.name.clone()).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pov_directions;

    #[test]
    fn pov_angles_to_directions() {
        assert_eq!(pov_directions(0xFFFF_FFFF), Vec::<u8>::new());
        assert_eq!(pov_directions(0), vec![0]);
        assert_eq!(pov_directions(9_000), vec![1]);
        assert_eq!(pov_directions(18_000), vec![2]);
        assert_eq!(pov_directions(27_000), vec![3]);
        assert_eq!(pov_directions(4_500), vec![0, 1]);
        assert_eq!(pov_directions(31_500), vec![0, 3]);
    }
}
