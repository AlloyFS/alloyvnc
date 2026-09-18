//! What DXGI says about this machine: every adapter, every output, and
//! whether duplication is allowed on it.
//!
//! The one diagnostic worth having when a capture refuses to start. It
//! names the adapter, the monitor and the HRESULT rather than leaving one
//! error line to be guessed at.
//!
//! `cargo run -p alloyvnc-screen-dxgi --example probe`

#[cfg(not(windows))]
fn main() {
    println!("DXGI is Windows only");
}

#[cfg(windows)]
fn main() {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1};
    use windows::core::Interface;

    alloyvnc_screen_dxgi::init();

    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.expect("a DXGI factory");
    for a in 0u32.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else {
            break;
        };
        let desc = unsafe { adapter.GetDesc1() }.expect("an adapter description");
        let name = String::from_utf16_lossy(&desc.Description).replace('\0', "");
        println!(
            "adapter {a}: {name} (vendor {:#06x}, device {:#06x})",
            desc.VendorId, desc.DeviceId
        );

        let mut device: Option<ID3D11Device> = None;
        let made = unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        let device = match (made, device) {
            (Ok(()), Some(device)) => device,
            (Err(e), _) => {
                println!("  no device: {e}");
                continue;
            }
            _ => continue,
        };

        for o in 0u32.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else {
                break;
            };
            let desc = unsafe { output.GetDesc() }.expect("a monitor description");
            let name = String::from_utf16_lossy(&desc.DeviceName).replace('\0', "");
            let r = desc.DesktopCoordinates;
            println!(
                "  output {o}: {name} attached={} rotation={} at {},{} to {},{}",
                desc.AttachedToDesktop.as_bool(),
                desc.Rotation.0,
                r.left,
                r.top,
                r.right,
                r.bottom
            );
            let output1: IDXGIOutput1 = match output.cast() {
                Ok(output1) => output1,
                Err(e) => {
                    println!("    no IDXGIOutput1: {e}");
                    continue;
                }
            };
            match unsafe { output1.DuplicateOutput(&device) } {
                Ok(dup) => {
                    let desc = unsafe { dup.GetDesc() };
                    println!(
                        "    duplication: {}x{} rotation {} in-memory={}",
                        desc.ModeDesc.Width,
                        desc.ModeDesc.Height,
                        desc.Rotation.0,
                        desc.DesktopImageInSystemMemory.as_bool()
                    );
                }
                Err(e) => println!("    duplication refused: {e} ({:#010x})", e.code().0),
            }
        }
    }
}
