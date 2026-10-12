use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Once;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use futures::StreamExt;
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tokio::sync::broadcast::{error::RecvError, Receiver as BReceiver};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::view::{border_frame, format_state, OverlayView, ViewParams};
use crate::app::AppUpdate;
use crate::settings::OverlaySettings;

const USER_CSS_PATH: &str = "iron-heart-overlay.css";
const BASE_CLASS: &str = "iron-heart-overlay";

const DEFAULT_CSS: &str = "
window.iron-heart-overlay-window,
window.iron-heart-overlay-border-window {
    background: transparent;
}

.iron-heart-overlay-border {
    color: #e01b24;
}

.iron-heart-overlay {
    background-color: rgba(0, 0, 0, 0.6);
    color: #ffffff;
    border-radius: 999px;
    padding: 6px 18px;
    font-size: 20px;
    font-weight: bold;
}

.iron-heart-overlay.disconnected {
    color: rgba(255, 255, 255, 0.5);
}

.iron-heart-overlay.stale {
    opacity: 0.6;
}

@keyframes iron-heart-flash {
    from { background-color: #c01c28; }
    to { background-color: rgba(0, 0, 0, 0.6); }
}

.iron-heart-overlay.alarm {
    animation: iron-heart-flash 0.5s ease-in-out infinite alternate;
}
";

enum OverlayMsg {
    Hr(u16),
    Quit,
}

/// Starts the overlay. Returns the (forwarder task, GTK thread) handles.
pub fn spawn_overlay(
    settings: OverlaySettings,
    mut broadcast_rx: BReceiver<AppUpdate>,
    cancel: CancellationToken,
) -> (JoinHandle<()>, JoinHandle<()>) {
    let (tx, rx) = unbounded::<OverlayMsg>();

    let forward_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                update = broadcast_rx.recv() => {
                    match update {
                        Ok(AppUpdate::HeartRateStatus(status)) => {
                            if tx.unbounded_send(OverlayMsg::Hr(status.heart_rate_bpm)).is_err() {
                                debug!("Overlay closed, stopping forwarder");
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(count)) => {
                            warn!("Overlay: Lagged! Missed {count} messages");
                        }
                        Err(RecvError::Closed) => {
                            error!("Overlay: Channel closed");
                            let _ = tx.unbounded_send(OverlayMsg::Quit);
                            return;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    info!("Overlay forwarder shutting down");
                    let _ = tx.unbounded_send(OverlayMsg::Quit);
                    return;
                }
            }
        }
    });

    // GTK gets one dedicated thread for the whole session
    let gtk_handle = tokio::task::spawn_blocking(move || run_gtk(settings, rx));

    (forward_handle, gtk_handle)
}

/// GTK/GLib would otherwise print warnings to stderr, which would corrupt the TUI.
fn route_glib_logs_to_tracing() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gtk::glib::log_set_writer_func(|level, fields| {
            let mut domain = "";
            let mut message = "";
            for field in fields {
                match field.key() {
                    "GLIB_DOMAIN" => domain = field.value_str().unwrap_or(""),
                    "MESSAGE" => message = field.value_str().unwrap_or(""),
                    _ => {}
                }
            }
            match level {
                gtk::glib::LogLevel::Error | gtk::glib::LogLevel::Critical => {
                    error!(target: "gtk", "{domain}: {message}")
                }
                gtk::glib::LogLevel::Warning => warn!(target: "gtk", "{domain}: {message}"),
                gtk::glib::LogLevel::Message | gtk::glib::LogLevel::Info => {
                    info!(target: "gtk", "{domain}: {message}")
                }
                _ => debug!(target: "gtk", "{domain}: {message}"),
            }
            gtk::glib::LogWriterOutput::Handled
        });
        gtk::glib::set_print_handler(|message| debug!(target: "gtk", "{}", message.trim_end()));
        gtk::glib::set_printerr_handler(|message| debug!(target: "gtk", "{}", message.trim_end()));
    });
}

fn parse_layer(layer: &str) -> Layer {
    match layer {
        "background" => Layer::Background,
        "bottom" => Layer::Bottom,
        "top" => Layer::Top,
        "overlay" => Layer::Overlay,
        other => {
            warn!("Unknown overlay.layer {other:?}, using \"overlay\"");
            Layer::Overlay
        }
    }
}

fn load_css(display: &gtk::gdk::Display) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(DEFAULT_CSS);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Working directory is already set by run_tui
    if Path::new(USER_CSS_PATH).exists() {
        let user_provider = gtk::CssProvider::new();
        user_provider.load_from_path(USER_CSS_PATH);
        gtk::style_context_add_provider_for_display(
            display,
            &user_provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );
        info!("Loaded overlay styles from {USER_CSS_PATH}");
    }
}

type AlarmStart = Rc<Cell<Option<Instant>>>;
/// Gradient start and end points (x0, y0, x1, y1)
type Line = (f64, f64, f64, f64);
type Quad = [(f64, f64); 4];

/// Full-screen gradient that is only shown (and animated) while the alarm is active.
struct BorderWindow {
    window: gtk::Window,
    area: gtk::DrawingArea,
    tick: RefCell<Option<gtk::TickCallbackId>>,
}

impl BorderWindow {
    fn set_active(&self, active: bool) {
        let mut tick = self.tick.borrow_mut();
        if active == tick.is_some() {
            return;
        }
        if active {
            self.window.present();
            *tick = Some(self.area.add_tick_callback(|area, _| {
                area.queue_draw();
                gtk::glib::ControlFlow::Continue
            }));
        } else {
            if let Some(id) = tick.take() {
                id.remove();
            }
            self.window.set_visible(false);
        }
    }

    fn close(&self) {
        self.set_active(false);
        self.window.close();
    }
}

struct OverlayWindow {
    monitor: gtk::gdk::Monitor,
    window: gtk::Window,
    label: gtk::Label,
    border: BorderWindow,
}

fn set_view(label: &gtk::Label, view: &OverlayView) {
    let mut classes = vec![BASE_CLASS];
    classes.extend(view.classes.iter().copied());
    label.set_label(&view.text);
    label.set_css_classes(&classes);
}

fn draw_border(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    elapsed: Duration,
) {
    let frame = border_frame(elapsed);
    let color = area.color();
    let (r, g, b) = (
        color.red() as f64,
        color.green() as f64,
        color.blue() as f64,
    );
    let alpha = frame.alpha * color.alpha() as f64;

    let (w, h) = (width as f64, height as f64);
    let t = frame.thickness.min(w / 2.0).min(h / 2.0);
    if t <= 0.0 {
        return;
    }

    // Each edge is a trapezoid with a gradient fading inward. The diagonal joins have equal
    // strength on both sides, so with antialiasing off no seam is visible in the corners.
    cr.set_antialias(gtk::cairo::Antialias::None);
    let edges: [(Line, Quad); 4] = [
        (
            (0.0, 0.0, 0.0, t),
            [(0.0, 0.0), (w, 0.0), (w - t, t), (t, t)],
        ),
        (
            (0.0, h, 0.0, h - t),
            [(0.0, h), (w, h), (w - t, h - t), (t, h - t)],
        ),
        (
            (0.0, 0.0, t, 0.0),
            [(0.0, 0.0), (t, t), (t, h - t), (0.0, h)],
        ),
        (
            (w, 0.0, w - t, 0.0),
            [(w, 0.0), (w, h), (w - t, h - t), (w - t, t)],
        ),
    ];
    for ((x0, y0, x1, y1), points) in edges {
        let gradient = gtk::cairo::LinearGradient::new(x0, y0, x1, y1);
        gradient.add_color_stop_rgba(0.0, r, g, b, alpha);
        gradient.add_color_stop_rgba(0.35, r, g, b, alpha * 0.45);
        gradient.add_color_stop_rgba(1.0, r, g, b, 0.0);
        let _ = cr.set_source(&gradient);
        cr.move_to(points[0].0, points[0].1);
        for (x, y) in &points[1..] {
            cr.line_to(*x, *y);
        }
        cr.close_path();
        if let Err(e) = cr.fill() {
            debug!("Couldn't draw overlay border: {e}");
            return;
        }
    }
}

fn build_border(
    settings: &OverlaySettings,
    monitor: &gtk::gdk::Monitor,
    alarm_start: &AlarmStart,
) -> BorderWindow {
    let window = gtk::Window::new();
    window.set_decorated(false);
    window.set_focusable(false);
    window.add_css_class("iron-heart-overlay-border-window");

    window.init_layer_shell();
    window.set_monitor(Some(monitor));
    window.set_namespace(Some("iron-heart-border"));
    window.set_layer(parse_layer(&settings.layer));
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_keyboard_mode(KeyboardMode::None);
    // Cover the whole monitor, even where panels reserve space
    window.set_exclusive_zone(-1);

    // Re-applied on every map, since the window is hidden between alarms
    window.connect_map(|window| match window.surface() {
        Some(surface) => surface.set_input_region(Some(&gtk::cairo::Region::create())),
        None => warn!("Overlay border has no surface, can't enable click-through"),
    });

    let area = gtk::DrawingArea::new();
    area.add_css_class("iron-heart-overlay-border");
    area.set_hexpand(true);
    area.set_vexpand(true);
    let alarm_start = alarm_start.clone();
    area.set_draw_func(move |area, cr, width, height| {
        if let Some(started) = alarm_start.get() {
            draw_border(area, cr, width, height, started.elapsed());
        }
    });
    window.set_child(Some(&area));

    BorderWindow {
        window,
        area,
        tick: RefCell::new(None),
    }
}

fn build_window(
    settings: &OverlaySettings,
    monitor: &gtk::gdk::Monitor,
    alarm_start: &AlarmStart,
) -> OverlayWindow {
    let window = gtk::Window::new();
    window.set_decorated(false);
    window.set_focusable(false);
    window.add_css_class("iron-heart-overlay-window");

    window.init_layer_shell();
    window.set_monitor(Some(monitor));
    window.set_namespace(Some("iron-heart"));
    window.set_layer(parse_layer(&settings.layer));
    // Anchoring to only the bottom edge keeps the window centered and content-sized
    window.set_anchor(Edge::Bottom, true);
    window.set_margin(Edge::Bottom, settings.margin_bottom);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_exclusive_zone(0);

    // Empty input region so clicks pass through to whatever is underneath
    window.connect_realize(|window| match window.surface() {
        Some(surface) => surface.set_input_region(Some(&gtk::cairo::Region::create())),
        None => warn!("Overlay has no surface, can't enable click-through"),
    });

    let label = gtk::Label::new(Some("♥ --"));
    label.add_css_class(BASE_CLASS);
    label.add_css_class("disconnected");
    window.set_child(Some(&label));

    OverlayWindow {
        monitor: monitor.clone(),
        window,
        label,
        border: build_border(settings, monitor, alarm_start),
    }
}

fn run_gtk(settings: OverlaySettings, mut rx: UnboundedReceiver<OverlayMsg>) {
    route_glib_logs_to_tracing();

    if let Err(e) = gtk::init() {
        warn!("Couldn't initialize GTK, overlay disabled: {e}");
        return;
    }
    if !gtk4_layer_shell::is_supported() {
        warn!("Compositor lacks wlr-layer-shell, overlay disabled");
        return;
    }
    let Some(display) = gtk::gdk::Display::default() else {
        warn!("No GDK display available, overlay disabled");
        return;
    };

    load_css(&display);

    // Plain windows + main loop (no gtk::Application), so there's no D-Bus name
    // or single-instance behavior to worry about
    let main_loop = gtk::glib::MainLoop::new(None, false);

    let params = ViewParams {
        stale_after: Duration::from_secs(settings.stale_secs as u64),
        low_bpm: settings.low_bpm,
        high_bpm: settings.high_bpm,
    };
    let last_bpm = Rc::new(Cell::new(0u16));
    let last_packet: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
    let applied: Rc<RefCell<Option<OverlayView>>> = Rc::new(RefCell::new(None));
    let windows: Rc<RefCell<Vec<OverlayWindow>>> = Rc::new(RefCell::new(Vec::new()));
    // When BPM first went out of range; None while in range (resets the border)
    let alarm_start: AlarmStart = Rc::new(Cell::new(None));
    let alarm_border = settings.alarm_border;

    // Only touches the widgets when something changed, since resetting
    // CSS classes would restart the alarm flash animation
    let apply = {
        let last_bpm = last_bpm.clone();
        let last_packet = last_packet.clone();
        let applied = applied.clone();
        let windows = windows.clone();
        let alarm_start = alarm_start.clone();
        Rc::new(move || {
            let age = last_packet.get().map(|at| at.elapsed());
            let view = format_state(last_bpm.get(), age, &params);
            if applied.borrow().as_ref() == Some(&view) {
                return;
            }
            let alarming = view.classes.contains(&"alarm");
            if !alarming {
                alarm_start.set(None);
            } else if alarm_start.get().is_none() {
                alarm_start.set(Some(Instant::now()));
            }
            for overlay in windows.borrow().iter() {
                set_view(&overlay.label, &view);
                overlay.border.set_active(alarm_border && alarming);
            }
            *applied.borrow_mut() = Some(view);
        })
    };

    // One overlay window per monitor, kept in sync as monitors come and go
    let sync_monitors = {
        let display = display.clone();
        let windows = windows.clone();
        let applied = applied.clone();
        let alarm_start = alarm_start.clone();
        Rc::new(move || {
            let monitors = display.monitors();
            let current: Vec<gtk::gdk::Monitor> = (0..monitors.n_items())
                .filter_map(|i| monitors.item(i).and_downcast::<gtk::gdk::Monitor>())
                .filter(|monitor| monitor.is_valid())
                .collect();

            let removed: Vec<OverlayWindow> = {
                let mut windows = windows.borrow_mut();
                let (keep, removed) = std::mem::take(&mut *windows)
                    .into_iter()
                    .partition(|overlay| current.contains(&overlay.monitor));
                *windows = keep;
                removed
            };
            for overlay in removed {
                debug!("Closing overlay for removed monitor");
                overlay.window.close();
                overlay.border.close();
            }

            for monitor in &current {
                if windows.borrow().iter().any(|o| &o.monitor == monitor) {
                    continue;
                }
                let connector = monitor.connector();
                debug!(
                    "Opening overlay on {}",
                    connector.as_deref().unwrap_or("unknown")
                );
                let overlay = build_window(&settings, monitor, &alarm_start);
                if let Some(view) = applied.borrow().as_ref() {
                    set_view(&overlay.label, view);
                }
                overlay.window.present();
                overlay
                    .border
                    .set_active(alarm_border && alarm_start.get().is_some());
                windows.borrow_mut().push(overlay);
            }
        })
    };
    display.monitors().connect_items_changed({
        let sync_monitors = sync_monitors.clone();
        move |_, _, _, _| sync_monitors()
    });
    sync_monitors();

    gtk::glib::spawn_future_local({
        let apply = apply.clone();
        let windows = windows.clone();
        let main_loop = main_loop.clone();
        async move {
            while let Some(msg) = rx.next().await {
                match msg {
                    OverlayMsg::Hr(bpm) => {
                        last_bpm.set(bpm);
                        last_packet.set((bpm > 0).then(Instant::now));
                        apply();
                    }
                    OverlayMsg::Quit => break,
                }
            }
            let all = std::mem::take(&mut *windows.borrow_mut());
            for overlay in all {
                overlay.window.close();
                overlay.border.close();
            }
            main_loop.quit();
        }
    });

    gtk::glib::timeout_add_local(Duration::from_secs(1), move || {
        apply();
        gtk::glib::ControlFlow::Continue
    });

    info!(
        "Layer-shell overlay started on {} monitor(s)",
        windows.borrow().len()
    );
    main_loop.run();
    info!("Layer-shell overlay closed");
}
