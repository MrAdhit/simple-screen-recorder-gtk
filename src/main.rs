use std::io::{Cursor, ErrorKind};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk::gdk::{MemoryFormat, MemoryTexture, Paintable};
use gtk::gio::{Cancellable, ListStore};
use gtk::glib::translate::UnsafeFrom as _;
use gtk::glib::Bytes;
use gtk::{prelude::*, ApplicationWindow, Button, FileDialog, FileFilter, Paned, Picture};
use gtk::{glib, Application};
use scrap::codec::{EncoderApi as _, EncoderCfg, Quality};
use scrap::{vpxcodec, Capturer, Display, Frame, TraitCapturer as _, TraitPixelBuffer as _, STRIDE_ALIGN};
use tokio::runtime::Runtime;
use tokio::sync::broadcast;
use webm::mux::Track as _;

const APP_ID: &str = "com.mradhit.SimpleScreenRecorder";

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_activate(build_ui);

    app.run()
}

fn async_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();

    RUNTIME.get_or_init(|| Runtime::new().expect("Can't create static async runtime"))
}

fn build_ui(application: &Application) {
    let picture = Picture::builder()
        .width_request(16 * 100)
        .height_request(9 * 100)
        .build();

    let capturer = ScreenCapturer::start_capture(0);

    glib::spawn_future_local({
        let picture = picture.clone();
        let mut screen_rx = capturer.frame_receiver.resubscribe();

        async move {
            while let Ok(frame) = screen_rx.recv().await {
                let Frame::PixelBuffer(buffer) = (unsafe { &*frame.load(Ordering::Acquire) }) else { continue };
    
                let bytes = Bytes::from(buffer.data());
                    
                let texture = MemoryTexture::new(buffer.width() as _, buffer.height() as _, MemoryFormat::B8g8r8a8, &bytes, buffer.stride()[0]);

                let paintable = unsafe { Paintable::unsafe_from(texture.into()) };
                
                picture.set_paintable(Some(&paintable));
            }
        }
    });

    let record_button = Button::builder()
        .label("Start")
        .build();

    let recorder = Arc::new(Mutex::new(None));
    let (finish_recording_tx, mut finish_recording_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    glib::spawn_future_local(async move {
        loop {
            let Some(data) = finish_recording_rx.recv().await else { continue };

            save_as("Save as", &["video/webm"], |at| {
                let Ok(at) = at else { return };
                std::fs::write(at.path().unwrap(), data).unwrap();
            });
        }
        
    });

    record_button.connect_clicked({
        let finish_recording_tx = finish_recording_tx.clone();
        let capturer = capturer.clone();
        let recorder = recorder.clone();

        move |button| {
            let mut recorder = recorder.lock().unwrap();
            
            if recorder.is_none() {
                let finish_recording_tx = finish_recording_tx.clone();
                *recorder = Some(capturer.start_recorder(move |data| {
                    println!("Saving data");
                    finish_recording_tx.send(data).unwrap();
                }));
                button.set_label("Stop");
            } else {
                recorder.as_ref().unwrap()();
                *recorder = None;
                button.set_label("Start");
            }
        }
    });
    
    let pane = Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(&picture)
        .end_child(&record_button)
        .build();

    let window = ApplicationWindow::builder()
        .application(application)
        .title("Simple Screen Recorder")
        .child(&pane)
        .build();

    window.present();
}

fn save_as<'a, P: FnOnce(Result<gtk::gio::File, glib::Error>) + 'static>(title: &'a str, mime_types: &'a [&'a str], callback: P) {
    let filters = ListStore::new::<FileFilter>();

    for mime_type in mime_types {
        let file_filter = FileFilter::new();
        file_filter.add_mime_type(mime_type);
    
        filters.append(&file_filter);
    }

    let window = FileDialog::builder()
        .title(title)
        .filters(&filters)
        .build();

    window.save(None::<&ApplicationWindow>, None::<&Cancellable>, callback);
}

struct ScreenCapturer {
    frame_receiver: broadcast::Receiver<Arc<AtomicPtr<Frame<'static>>>>,
    capturer_ptr: Arc<AtomicPtr<Capturer>>,
}

impl Clone for ScreenCapturer {
    fn clone(&self) -> Self {
        Self { frame_receiver: self.frame_receiver.resubscribe(), capturer_ptr: self.capturer_ptr.clone() }
    }
}

impl ScreenCapturer {
    fn start_recorder<F: FnOnce(Vec<u8>) + Send + 'static>(&self, on_finish: F) -> impl Fn() {
        let capturer = unsafe { &*self.capturer_ptr.load(Ordering::Acquire) };
        let (width, height) = (capturer.width(), capturer.height());

        let should_stop_ptr = Arc::new(AtomicPtr::new(Box::into_raw(Box::new(false))));

        async_runtime().spawn({
            let mut frame_rx = self.frame_receiver.resubscribe();
            let should_stop_ptr = should_stop_ptr.clone();

            async move {
                let mut res = Cursor::new(Vec::new());
                let mut mux = webm::mux::Segment::new(webm::mux::Writer::new(&mut res)).unwrap();

                let (vpx_codec, mux_codec) = (vpxcodec::VpxVideoCodecId::VP9, webm::mux::VideoCodecId::VP9);

                let mut video_track = mux.add_video_track(width as _, height as _, None, mux_codec);

                let mut encoder = vpxcodec::VpxEncoder::new(
                    EncoderCfg::VPX(vpxcodec::VpxEncoderConfig {
                        width: width as _,
                        height: height as _,
                        quality: Quality::Best,
                        codec: vpx_codec,
                        keyframe_interval: None,
                    }),
                    false,
                ).unwrap();

                println!("Starting recorder");

                let mut yuv = Vec::new();
                let mut mid_data = Vec::new();

                let start = Instant::now();

                loop {
                    let Ok(frame) = frame_rx.recv().await else { continue };

                    let should_stop = unsafe { *should_stop_ptr.load(Ordering::Acquire) };

                    let frame_time_now = Instant::now();
                    let frame_time = frame_time_now - start;

                    let frame = unsafe { &mut *frame.load(Ordering::Acquire) };

                    let ms = frame_time.as_millis();

                    frame.to(encoder.yuvfmt(), &mut yuv, &mut mid_data).unwrap();

                    for frame in encoder.encode(ms as _, &yuv, STRIDE_ALIGN).unwrap() {
                        video_track.add_frame(frame.data, (frame.pts * 1_000_000) as _, frame.key);
                    }

                    if should_stop {
                        println!("Stopping recorder");
                        break
                    };
                }

                mux.finalize(None);

                on_finish(res.into_inner());
            }
        });

        move || unsafe {
            *should_stop_ptr.load(Ordering::Acquire) = true
        }
    }

    fn start_capture(display_index: usize) -> Self {
        let display = Display::all().unwrap().remove(display_index);

        let capturer_ptr = Arc::new(AtomicPtr::new(Box::into_raw(Box::new(Capturer::new(display).unwrap()))));

        let (frame_sender, frame_receiver) = broadcast::channel(1024);

        std::thread::spawn({
            let capturer_ptr = capturer_ptr.clone();
            let frame_sender = frame_sender;

            move || {
                let mut capturer = unsafe { *Box::from_raw(*capturer_ptr.as_ptr()) };
                let mut last_frame = None;

                loop {
                    match capturer.frame(Duration::ZERO) {
                        Ok(frame) => {
                            last_frame = Some(Arc::new(AtomicPtr::new(Box::into_raw(Box::new(frame)) as *mut Frame<'static>)));
                        },
                        Err(ref error) if error.kind() == ErrorKind::WouldBlock => { },
                        Err(err) => panic!("{err}"),
                    }

                    let Ok(_) = frame_sender.send(last_frame.clone().unwrap()) else { break };
                }

                anyhow::Ok(())
            }
        });

        Self {
            frame_receiver,
            capturer_ptr,
        }
    }
}
