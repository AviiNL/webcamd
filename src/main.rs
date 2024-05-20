#![feature(stdarch_arm_neon_intrinsics)]

use imageproc::{
    drawing::{draw_text_mut, text_size},
    rect::Rect,
};
use std::{
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, SystemTime},
};
use time::OffsetDateTime;
use tokio::{io::AsyncWriteExt, net::TcpListener, signal, sync::RwLock, time::sleep};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

use eye_hal::traits::{Context, Device, Stream};
use eye_hal::PlatformContext;

#[tokio::main]
async fn main() -> Result<()> {
    // todo: Make CLI Arguments
    let uri = "/dev/video0";
    let width = "1280";
    let height = "720";
    let fps = "24";
    let octoprint_api_key = "E76C292A8D674721B60910890923CE77";

    let pixel_format = eye_hal::format::PixelFormat::Jpeg;
    let quality: i32 = "100".parse()?;

    let fps_as_ms = 1000.0 / fps.parse::<f64>()?;
    let interval = Duration::try_from_secs_f64(fps_as_ms / 1000.0)?;

    let frame_data = Arc::new(RwLock::new(Vec::<u8>::new()));

    let abort = Arc::new(AtomicBool::new(false));

    let status_text = Arc::new(RwLock::new("Idle".to_string()));

    let temp_text = Arc::new(RwLock::new("CPU Temp: 0.0'C".to_string()));

    let temp_writer = temp_text.clone();
    let aborter = abort.clone();
    tokio::spawn(async move {
        loop {
            if aborter.load(std::sync::atomic::Ordering::SeqCst) {
                println!("Exit");
                break;
            }

            let mut input = tokio::process::Command::new("vcgencmd");
            input.arg("measure_temp");

            {
                *temp_writer.write().await =
                    String::from_utf8(input.output().await.expect("this not to fail").stdout)
                        .expect("this to be valid utf8")
                        .trim()
                        .replace("temp", "CPU Temp")
                        .replace('=', ": ");
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let status_writer = status_text.clone();
    let aborter = abort.clone();
    tokio::spawn(async move {
        loop {
            if aborter.load(std::sync::atomic::Ordering::SeqCst) {
                println!("Exit");
                break;
            }

            let Ok(res) = reqwest::Client::new()
                .get("http://192.168.1.111/api/job")
                .header("Authorization", &format!("Bearer {}", octoprint_api_key))
                .send()
                .await
            else {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            };

            let Ok(res) = res.json::<serde_json::Value>().await else {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            };

            let state = res["state"].as_str().unwrap_or("Error");
            let progress = res["progress"]["completion"].as_f64().unwrap_or(0.0);

            let progress = progress.round() as i64;

            {
                *status_writer.write().await = format!("{}: {}%", state, progress);
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let aborter = abort.clone();
    let fd = frame_data.clone();
    tokio::spawn(async move {
        let Ok(listener) = TcpListener::bind("0.0.0.0:8080").await else {
            return;
        };

        while let Ok((mut stream, _)) = listener.accept().await {
            let fd = fd.clone();
            let a2 = aborter.clone();
            tokio::spawn(async move {
                let response = "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n";

                if let Err(e) = stream.write_all(response.as_bytes()).await {
                    eprintln!("{:?}", e);
                }

                loop {
                    if a2.load(std::sync::atomic::Ordering::SeqCst) {
                        println!("Exit");
                        break;
                    }

                    // Every new connection to this 'stream' on every frame, it locks the frame data
                    // The more 'watchers' there are, the more time is spent with this frame data locked
                    // This needs to be optimized in some way shape or form.
                    let lock = fd.read().await;
                    let data = lock.clone();
                    drop(lock);

                    let image_data = format!(
                        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                        data.len()
                    );

                    if let Err(e) = stream.write_all(image_data.as_bytes()).await {
                        eprintln!("Error: {:?}", e);
                        break;
                    }
                    if let Err(e) = stream.write_all(&data).await {
                        eprintln!("Error: {:?}", e);
                        break;
                    }
                    if let Err(e) = stream.write_all(b"\r\n").await {
                        eprintln!("Error: {:?}", e);
                        break;
                    }
                    if let Err(e) = stream.flush().await {
                        eprintln!("Error: {:?}", e);
                        break;
                    }

                    sleep(interval).await;
                }
            });
        }
    });

    let aborter = abort.clone();
    tokio::spawn(async move {
        // Create a context
        let ctx = PlatformContext::default();

        // Find selected device
        let Some(device) = ctx
            .devices()
            .expect("to get devices")
            .into_iter()
            .find(|f| f.uri.ends_with(uri))
        else {
            panic!("Device {} not found", uri);
        };

        let device = ctx.open_device(&device.uri).expect("to open the device");
        let descriptors = device.streams().expect("to read descriptors");

        let Some(descriptor) = descriptors.into_iter().find(|a| {
            let w = width.parse::<u32>().unwrap();
            let h = height.parse::<u32>().unwrap();
            let i = interval;

            a.width == w && a.height == h && a.interval == i && a.pixfmt == pixel_format
        }) else {
            panic!("Format {}x{}@{} not found", width, height, fps);
        };

        let mut stream = device
            .start_stream(&descriptor)
            .expect("Unable to start stream");

        let format = time::format_description::parse("[hour]:[minute]:[second]")
            .expect("to have the correct format");

        loop {
            if aborter.load(std::sync::atomic::Ordering::SeqCst) {
                println!("Exit");
                break;
            }

            let Some(Ok(next_frame)) = stream.next() else {
                eprintln!("Failed to grab frame");
                sleep(interval).await;
                continue;
            };

            let frame =
                if descriptor.pixfmt == eye_hal::format::PixelFormat::Custom("YUYV".to_string()) {
                    let Ok(frame) = yuv_to_image(next_frame, descriptor.width, descriptor.height)
                    else {
                        eprintln!("Failed to decompress jpeg");
                        sleep(Duration::from_millis(1)).await;
                        continue;
                    };

                    frame
                } else {
                    let Ok(frame) = jpeg_to_image(next_frame) else {
                        eprintln!("Failed to decompress jpeg");
                        sleep(Duration::from_millis(1)).await;
                        continue;
                    };

                    frame
                };

            let ratio = 16.0 / 9.0;
            let xoffset = 0.015;
            let yoffset = xoffset * ratio;

            let dt: OffsetDateTime = SystemTime::now().into();
            let dt = dt.to_offset(time::UtcOffset::from_hms(2, 0, 0).unwrap());
            let time = &dt.format(&format).expect("to format time");

            let status = { status_text.read().await }.clone();
            let temp = { temp_text.read().await }.clone();

            let frame = draw_text(frame, time, 1.0 - xoffset, yoffset)
                .await
                .expect("to draw time");

            let frame = draw_text(frame, &temp, xoffset, 1.0 - yoffset)
                .await
                .expect("to draw temp");

            let frame = draw_text(frame, &status, xoffset, yoffset)
                .await
                .expect("to draw status");

            let buffer =
                turbojpeg::compress_image(&frame.into_rgb8(), quality, turbojpeg::Subsamp::Sub2x2)
                    .expect("to compress image")
                    .to_vec();

            *frame_data.write().await = buffer;
        }
    });

    signal::ctrl_c().await?;

    abort.store(true, std::sync::atomic::Ordering::SeqCst);

    Ok(())
}

fn yuv_to_image(frame: &[u8], w: u32, h: u32) -> Result<image::DynamicImage> {
    let frame = frame.chunks_exact(4).fold(vec![], |mut acc, v| {
        // convert form YUYV to RGB
        let [y, u, _, v]: [u8; 4] = std::convert::TryFrom::try_from(v).unwrap();
        let y = y as f32;
        let u = u as f32;
        let v = v as f32;

        let r = 1.164 * (y - 16.) + 1.596 * (v - 128.);
        let g = 1.164 * (y - 16.) - 0.813 * (v - 128.) - 0.391 * (u - 128.);
        let b = 1.164 * (y - 16.) + 2.018 * (u - 128.);

        let r = r as u8;
        let g = g as u8;
        let b = b as u8;

        acc.push(r);
        acc.push(g);
        acc.push(b);

        acc.push(r);
        acc.push(g);
        acc.push(b);

        acc
    });

    let image = image::ImageBuffer::<image::Rgb<u8>, Vec<u8>>::from_raw(w, h, frame)
        .ok_or("error creating image buffer")?;

    Ok(image::DynamicImage::ImageRgb8(image))
}

fn jpeg_to_image(frame: &[u8]) -> Result<image::DynamicImage> {
    let image = turbojpeg::decompress_image::<image::Rgb<u8>>(frame)?;
    let b = image::DynamicImage::ImageRgb8(image);

    Ok(b)
}

async fn draw_text(
    mut frame: image::DynamicImage,
    text: &str,
    x: f64,
    y: f64,
) -> Result<image::DynamicImage> {
    use rusttype::Font;
    use rusttype::Scale;

    let font_data: &[u8] = include_bytes!("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf");
    let font: Font<'static> = Font::try_from_bytes(font_data).unwrap();
    let scale = Scale::uniform(32.0);

    let border = 5;

    let white = image::Rgba([255u8, 255u8, 255u8, 255u8]);
    let black = image::Rgba([0u8, 0u8, 0u8, 128u8]);

    let frame_width = frame.width() as f64;
    let frame_height = frame.height() as f64;

    // calculate x and y with fractions and offsets
    let true_x = (frame_width / 100.0) * (x * 100.0);
    let true_y = (frame_height / 100.0) * (y * 100.0);

    let (text_width, text_height) = text_size(scale, &font, text);

    let offset_x = (text_width as f64 / 100.0) * (x * 100.0);
    let offset_y = (text_height as f64 / 100.0) * (y * 100.0);

    let x = (true_x - offset_x) as i32;
    let y = (true_y - offset_y) as i32;

    draw_blended_rect_mut(
        &mut frame,
        Rect::at(x - border + 1, y - border + 2).of_size(
            (text_width + (border * 2)) as u32,
            (text_height + (border * 2)) as u32,
        ),
        black,
    );

    draw_text_mut(&mut frame, white, x, y, scale, &font, text);

    Ok(frame)
}

pub fn draw_blended_rect_mut<I>(image: &mut I, rect: Rect, color: I::Pixel)
where
    I: image::GenericImage,
    I::Pixel: 'static,
{
    use image::Pixel;

    let image_bounds = Rect::at(0, 0).of_size(image.width(), image.height());
    if let Some(intersection) = image_bounds.intersect(rect) {
        for dy in 0..intersection.height() {
            for dx in 0..intersection.width() {
                let x = intersection.left() as u32 + dx;
                let y = intersection.top() as u32 + dy;
                let mut pixel = image.get_pixel(x, y); // added
                pixel.blend(&color); // added
                unsafe {
                    image.unsafe_put_pixel(x, y, pixel); // changed
                }
            }
        }
    }
}
