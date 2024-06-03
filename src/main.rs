use std::io::{Cursor, ErrorKind, Write};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk::gdk::{MemoryFormat, MemoryTexture, Paintable};
use gtk::gio::{Cancellable, ListStore};
use gtk::glib::translate::UnsafeFrom as _;
use gtk::glib::Bytes;
use gtk::{prelude::*, ApplicationWindow, Button, FileDialog, FileFilter, Paned, Picture};
use gtk::{glib, Application};
use scrap::{Capturer, Display, Frame, TraitCapturer as _, TraitPixelBuffer as _};
use tokio::runtime::Runtime;
use tokio::sync::broadcast;

use gstreamer::prelude::*;

const APP_ID: &str = "com.mradhit.SimpleScreenRecorder";

fn main() -> glib::ExitCode {
    gstreamer::init().unwrap();

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
                let data = Arc::new(Mutex::new(Cursor::new(Vec::new())));

                println!("Setting up encoding pipeline");

                let cpu_count = num_cpus::get();

                let launch = gstreamer::parse::launch(&format!("
                    appsrc name=input ! rawvideoparse width={width} height={height} format=8 ! videoconvert !
                    queue leaky=1 max-size-buffers=0 max-size-time=15000000000 max-size-bytes=104857600 !
                    vp8enc cpu-used={cpu_count} target-bitrate=80000000 ! webmmux ! appsink name=output
                ")).unwrap();

                let pipeline = launch.dynamic_cast::<gstreamer::Pipeline>().unwrap();

                let input_video_info = gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::Bgrx, width as _, height as _)
                    .build().unwrap();
                
                let input_source =
                    pipeline.by_name("input").unwrap()
                        .dynamic_cast::<gstreamer_app::AppSrc>().unwrap();

                input_source.set_caps(Some(&input_video_info.to_caps().unwrap()));

                let output_sink =
                    pipeline.by_name("output").unwrap()
                        .dynamic_cast::<gstreamer_app::AppSink>().unwrap();

                let output_callbacks =
                    gstreamer_app::AppSinkCallbacks::builder()
                        .new_sample({
                            let data = data.clone();

                            move |sink| {
                                let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                                let buffer = sample.buffer().unwrap();
    
                                let mapped = buffer.map_readable().unwrap();
    
                                data.lock().unwrap().write(&mapped).unwrap();
    
                                Ok(gstreamer::FlowSuccess::Ok)
                            }
                        })
                        .build();

                output_sink.set_callbacks(output_callbacks);

                println!("Starting recorder");

                let start = Instant::now();

                let input_callbacks =
                    gstreamer_app::AppSrcCallbacks::builder()
                        .need_data(move |source, _| {
                            let mut buffer = gstreamer::Buffer::with_size(input_video_info.size()).unwrap();

                            loop {
                                let should_stop = unsafe { *should_stop_ptr.load(Ordering::Acquire) };

                                if should_stop {
                                    println!("Stopping recorder...");
                                    source.end_of_stream().unwrap();
                                    break;
                                }
                                
                                let Ok(frame) = frame_rx.try_recv() else { continue };

                                let frame = unsafe { &mut *frame.load(Ordering::Acquire) };

                                let scrap::Frame::PixelBuffer(frame) = frame else { continue };

                                let frame_time = Instant::now() - start;
                                buffer.get_mut().unwrap().set_pts(Some(gstreamer::ClockTime::from_seconds_f64(frame_time.as_secs_f64())));
                                buffer.get_mut().unwrap().copy_from_slice(0, frame.data()).unwrap();

                                break;
                            }

                            let _ = source.push_buffer(buffer);
                        })
                        .build();
                
                input_source.set_callbacks(input_callbacks);

                pipeline.set_state(gstreamer::State::Playing).unwrap();

                let bus = pipeline.bus().unwrap();

                for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
                    match msg.view() {
                        gstreamer::MessageView::Eos(_) => break,
                        gstreamer::MessageView::Error(err) => eprintln!("{err}"),
                        _ => { }
                    }
                }

                pipeline.set_state(gstreamer::State::Null).unwrap();

                on_finish(data.lock().unwrap().clone().into_inner());
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
