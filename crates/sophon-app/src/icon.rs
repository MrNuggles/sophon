//! The Sophon mark, drawn in code: a glowing particle inside a thin ring.
//! Used for the window, the tray and the installed launcher icon.

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// RGBA pixels of the icon at `size` x `size`.
pub fn pixels(size: u32) -> Vec<u8> {
    let (accent, core) = ([143.0, 125.0, 255.0], [255.0, 255.0, 255.0]);
    let c = size as f32 / 2.0;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            // Supersample 4x4 for smooth edges.
            let (mut rgb, mut alpha) = ([0f32; 3], 0f32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let dx = (x as f32 + (sx as f32 + 0.5) / 4.0 - c) / c;
                    let dy = (y as f32 + (sy as f32 + 0.5) / 4.0 - c) / c;
                    let d = (dx * dx + dy * dy).sqrt();
                    let (col, a) = if d < 0.52 {
                        // Core: white centre fading to accent.
                        let t = (d / 0.52).powf(1.4);
                        (lerp(core, accent, t), 1.0)
                    } else if d < 0.66 {
                        // Soft glow.
                        (accent, 0.35 * (1.0 - (d - 0.52) / 0.14))
                    } else if (0.84..0.97).contains(&d) {
                        // Ring.
                        (accent, 0.75)
                    } else {
                        (accent, 0.0)
                    };
                    for i in 0..3 {
                        rgb[i] += col[i] * a;
                    }
                    alpha += a;
                }
            }
            let a = alpha / 16.0;
            for v in rgb {
                out.push(if alpha > 0.0 { (v / alpha).min(255.0) as u8 } else { 0 });
            }
            out.push((a * 255.0) as u8);
        }
    }
    out
}

pub fn image(size: u32) -> Image {
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&pixels(size), size, size);
    Image::from_rgba8(buf)
}

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Adds Sophon to the app launcher (`--install`) so desktops show its name and
/// icon in the taskbar, or removes it again (`--uninstall`).
pub fn install(remove: bool) -> std::io::Result<()> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    let desktop = data.join("applications/sophon.desktop");
    let icons: Vec<_> = [48u32, 128, 256]
        .iter()
        .map(|&s| (s, data.join(format!("icons/hicolor/{s}x{s}/apps/sophon.png"))))
        .collect();
    if remove {
        for p in std::iter::once(&desktop).chain(icons.iter().map(|(_, p)| p)) {
            match std::fs::remove_file(p) {
                Ok(()) => println!("removed {}", p.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        return Ok(());
    }
    for (size, path) in &icons {
        std::fs::create_dir_all(path.parent().expect("has parent"))?;
        crate::png::write(path, *size, *size, &pixels(*size))?;
        println!("wrote {}", path.display());
    }
    let exe = std::env::current_exe()?;
    if exe.to_string_lossy().contains(|c: char| c.is_control()) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "executable path contains control characters"));
    }
    std::fs::create_dir_all(desktop.parent().expect("has parent"))?;
    std::fs::write(
        &desktop,
        format!(
            "[Desktop Entry]\nType=Application\nName=Sophon\nGenericName=Task Manager\n\
Comment=See and control what your system is doing\nExec={}\nIcon=sophon\nTerminal=false\n\
Categories=System;Monitor;\nKeywords=task;process;monitor;cpu;gpu;memory;disk;\nStartupWMClass=sophon\n",
            exe.display()
        ),
    )?;
    println!("wrote {}", desktop.display());
    Ok(())
}
