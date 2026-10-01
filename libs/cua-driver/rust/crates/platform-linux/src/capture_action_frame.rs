#[cfg(target_os = "linux")]
use cua_driver_core::capture_runtime::{CaptureActionError, CaptureId};
use cua_driver_core::capture_runtime::{
    CaptureActionRequest, CapturePublication, CaptureService, CaptureTarget,
    EncodedScreenshotDimensions, NativeActionDimensions, ScreenshotToActionTransform,
};
use serde_json::Value;

fn window_target(pid: u32, window_id: u64) -> CaptureTarget {
    CaptureTarget::Window { pid, window_id }
}

fn publish(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    target: CaptureTarget,
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
    screenshot_to_action: ScreenshotToActionTransform,
) -> anyhow::Result<String> {
    let binding = service.binding_from_args(args)?;
    let capture_id = service.publish(CapturePublication {
        png_bytes: png_bytes.to_vec(),
        target,
        encoded_dimensions: EncodedScreenshotDimensions::new(
            encoded_dimensions.0,
            encoded_dimensions.1,
        )?,
        native_action_dimensions: NativeActionDimensions::new(
            native_action_dimensions.0,
            native_action_dimensions.1,
        )?,
        screenshot_to_action,
        session_id: binding.session_id().into(),
        session_generation: binding.session_generation(),
    })?;
    Ok(capture_id.to_string())
}

pub fn publish_window(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    pid: u32,
    window_id: u64,
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
) -> anyhow::Result<String> {
    publish(
        service,
        args,
        png_bytes,
        window_target(pid, window_id),
        encoded_dimensions,
        native_action_dimensions,
        scaled_transform(encoded_dimensions, native_action_dimensions)?,
    )
}

/// Hyprland desktop frame each desktop capture was taken of. Capture admission
/// otherwise compares only dimensions, which two different monitor layouts of
/// equal size (for example either of two equal monitors on its own) share.
/// Keyed by the parsed ID, so any accepted spelling of an ID finds its frame.
#[cfg(target_os = "linux")]
static DESKTOP_CAPTURE_FRAMES: std::sync::Mutex<
    std::collections::VecDeque<(CaptureId, crate::wayland::hyprland::DesktopFrame)>,
> = std::sync::Mutex::new(std::collections::VecDeque::new());

#[cfg(target_os = "linux")]
const REMEMBERED_DESKTOP_CAPTURES: usize = 64;

#[cfg(target_os = "linux")]
fn desktop_capture_frames() -> std::sync::MutexGuard<
    'static,
    std::collections::VecDeque<(CaptureId, crate::wayland::hyprland::DesktopFrame)>,
> {
    DESKTOP_CAPTURE_FRAMES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(target_os = "linux")]
pub fn remember_desktop_frame(
    capture_id: &str,
    frame: crate::wayland::hyprland::DesktopFrame,
) -> anyhow::Result<()> {
    let capture_id: CaptureId = capture_id.parse()?;
    let mut frames = desktop_capture_frames();
    frames.retain(|(id, _)| *id != capture_id);
    if frames.len() >= REMEMBERED_DESKTOP_CAPTURES {
        frames.pop_front();
    }
    frames.push_back((capture_id, frame));
    Ok(())
}

/// Admit a desktop click addressed in the Hyprland desktop frame `current`.
///
/// The capture is looked up without being consumed first, so an invalid,
/// unknown, expired or other-session ID keeps its own refusal code. A desktop
/// capture taken of a different monitor layout than `current`, or whose frame
/// is no longer recorded, is then refused with `capture_frame_mismatch` and
/// stays unconsumed. Admission compares the capture against `current`'s own
/// dimensions, so the whole action uses one compositor snapshot. Off Hyprland
/// (`current` is `None`) this is [`admit_desktop_click`].
#[cfg(target_os = "linux")]
pub fn admit_desktop_click_in_frame(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    current: Option<&crate::wayland::hyprland::DesktopFrame>,
    screenshot_x: f64,
    screenshot_y: f64,
) -> anyhow::Result<(f64, f64)> {
    let Some(current) = current else {
        return admit_desktop_click(service, args, capture_id, screenshot_x, screenshot_y);
    };
    let binding = service.binding_from_args(args)?;
    let id = service.parse_capture_id(capture_id)?;
    let capture = service
        .read_for_perception(id, &binding)
        .map_err(CaptureActionError::from)?;
    // A window capture is refused by admission with its target code.
    if *capture.target() == CaptureTarget::PrimaryDesktop {
        let recorded = desktop_capture_frames()
            .iter()
            .find(|(recorded, _)| *recorded == id)
            .map(|(_, frame)| frame.clone());
        match recorded {
            Some(frame) if frame == *current => {}
            Some(_) => {
                return Err(desktop_frame_mismatch(
                    id,
                    "was taken of a different monitor layout",
                ))
            }
            None => {
                return Err(desktop_frame_mismatch(
                    id,
                    "has no recorded Hyprland desktop frame",
                ))
            }
        }
    }
    let point = admit_with_live_dimensions(
        service,
        args,
        capture_id,
        CaptureTarget::PrimaryDesktop,
        NativeActionDimensions::new(current.width, current.height)?,
        screenshot_x,
        screenshot_y,
    )?;
    // Admission consumed the capture; its frame is no longer needed.
    desktop_capture_frames().retain(|(recorded, _)| *recorded != id);
    Ok(point)
}

#[cfg(target_os = "linux")]
fn desktop_frame_mismatch(capture_id: CaptureId, reason: &str) -> anyhow::Error {
    anyhow::Error::new(CaptureActionError::NativeActionFrameMismatch).context(format!(
        "capture {capture_id} {reason}; call get_desktop_state again"
    ))
}

pub fn publish_desktop(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
) -> anyhow::Result<String> {
    // The desktop screenshot can be downsized below the action frame.
    publish(
        service,
        args,
        png_bytes,
        CaptureTarget::PrimaryDesktop,
        encoded_dimensions,
        native_action_dimensions,
        scaled_transform(encoded_dimensions, native_action_dimensions)?,
    )
}

/// Screenshot-to-action scaling for a capture encoded at `encoded` pixels of
/// a `native` action frame. The resizer preserves aspect ratio before rounding
/// each encoded axis, so both ratios are derived independently: a one-pixel
/// rounded height must not skew Y coordinates.
fn scaled_transform(
    encoded: (u32, u32),
    native: (u32, u32),
) -> anyhow::Result<ScreenshotToActionTransform> {
    anyhow::ensure!(
        encoded.0 > 0 && encoded.1 > 0,
        "capture has an empty encoded frame: {}x{}",
        encoded.0,
        encoded.1
    );
    let scale_x = f64::from(native.0) / f64::from(encoded.0);
    let scale_y = f64::from(native.1) / f64::from(encoded.1);
    Ok(ScreenshotToActionTransform::new(
        scale_x, 0.0, 0.0, scale_y, 0.0, 0.0,
    )?)
}

fn admit(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    target: CaptureTarget,
    screenshot_x: f64,
    screenshot_y: f64,
) -> anyhow::Result<(f64, f64)> {
    let current_native_action_dimensions = live_action_dimensions(&target)?;
    admit_with_live_dimensions(
        service,
        args,
        capture_id,
        target,
        current_native_action_dimensions,
        screenshot_x,
        screenshot_y,
    )
}

fn admit_with_live_dimensions(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    target: CaptureTarget,
    current_native_action_dimensions: NativeActionDimensions,
    screenshot_x: f64,
    screenshot_y: f64,
) -> anyhow::Result<(f64, f64)> {
    let binding = service.binding_from_args(args)?;
    let capture_id = service.parse_capture_id(capture_id)?;
    let admission = service.admit_action(CaptureActionRequest {
        capture_id,
        binding,
        target,
        current_native_action_dimensions,
        screenshot_x,
        screenshot_y,
    })?;
    Ok((admission.action_x, admission.action_y))
}

#[cfg(target_os = "linux")]
fn live_action_dimensions(target: &CaptureTarget) -> anyhow::Result<NativeActionDimensions> {
    let dimensions = match target {
        CaptureTarget::Window { pid, window_id } => {
            let identity_matches = if crate::wayland::is_wayland() {
                crate::wayland::window_was_listed_for_pid(*pid, *window_id)
            } else {
                crate::x11::window_belongs_to_pid(*window_id, *pid)
            };
            anyhow::ensure!(
                identity_matches,
                "native window identity changed after capture"
            );
            let png = crate::wayland::screenshot_dispatch_with_pid(*window_id, *pid)?;
            crate::capture::png_dimensions_pub(&png)?
        }
        CaptureTarget::PrimaryDesktop => {
            let png = crate::capture::screenshot_display_bytes()?;
            let native = crate::capture::png_dimensions_pub(&png)?;
            desktop_action_dimensions(native)?
        }
    };
    Ok(NativeActionDimensions::new(dimensions.0, dimensions.1)?)
}

#[cfg(not(target_os = "linux"))]
fn live_action_dimensions(_target: &CaptureTarget) -> anyhow::Result<NativeActionDimensions> {
    anyhow::bail!("live Linux capture validation is unavailable on this platform")
}

#[cfg(target_os = "linux")]
pub(crate) fn desktop_action_dimensions(native: (u32, u32)) -> anyhow::Result<(u32, u32)> {
    let logical = if crate::wayland::is_wayland() {
        crate::wayland::compositor_logical_frame().transpose()?
    } else {
        None
    };
    select_desktop_action_dimensions(native, logical)
}

fn select_desktop_action_dimensions(
    native: (u32, u32),
    compositor_logical: Option<(u32, u32)>,
) -> anyhow::Result<(u32, u32)> {
    let dimensions = compositor_logical.unwrap_or(native);
    anyhow::ensure!(
        dimensions.0 > 0 && dimensions.1 > 0,
        "desktop action frame is empty: {}x{}",
        dimensions.0,
        dimensions.1
    );
    Ok(dimensions)
}

pub fn admit_window_click(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    pid: u32,
    window_id: u64,
    screenshot_x: f64,
    screenshot_y: f64,
) -> anyhow::Result<(f64, f64)> {
    admit(
        service,
        args,
        capture_id,
        window_target(pid, window_id),
        screenshot_x,
        screenshot_y,
    )
}

pub fn admit_desktop_click(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    screenshot_x: f64,
    screenshot_y: f64,
) -> anyhow::Result<(f64, f64)> {
    admit(
        service,
        args,
        capture_id,
        CaptureTarget::PrimaryDesktop,
        screenshot_x,
        screenshot_y,
    )
}

pub fn retire_runtime(service: &CaptureService) {
    service.retire_runtime();
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::capture_runtime::admission_error_code;
    use sha2::{Digest, Sha256};

    #[cfg(target_os = "linux")]
    fn frame_at(x: i32) -> crate::wayland::hyprland::DesktopFrame {
        crate::wayland::hyprland::DesktopFrame {
            x,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            outputs: Vec::new(),
        }
    }

    /// Serializes the tests that share the process-wide frame table, so the
    /// bounded-table test cannot evict another test's records.
    #[cfg(target_os = "linux")]
    static FRAME_TABLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(target_os = "linux")]
    fn admit_in(
        service: &CaptureService,
        args: &Value,
        capture_id: &str,
        frame: &crate::wayland::hyprland::DesktopFrame,
    ) -> anyhow::Result<(f64, f64)> {
        admit_desktop_click_in_frame(service, args, capture_id, Some(frame), 2.0, 1.0)
    }

    #[cfg(target_os = "linux")]
    fn refusal_code(result: anyhow::Result<(f64, f64)>) -> &'static str {
        admission_error_code(&result.expect_err("admission must refuse"))
    }

    /// Publishes a Hyprland desktop capture of `frame` the way
    /// `get_desktop_state` does: the capture's action frame is the frame's size.
    #[cfg(target_os = "linux")]
    fn publish_in(
        service: &CaptureService,
        args: &Value,
        frame: &crate::wayland::hyprland::DesktopFrame,
    ) -> String {
        let size = (frame.width, frame.height);
        let id = publish_desktop(service, args, &png(4, 3, 0x51), (4, 3), size).unwrap();
        remember_desktop_frame(&id, frame.clone()).unwrap();
        id
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_desktop_capture_of_another_layout_is_refused_without_being_consumed() {
        let _table = FRAME_TABLE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let service = CaptureService::default();
        let call_args = args("desktop-frame");
        let id = publish_in(&service, &call_args, &frame_at(-1920));

        // Same size, different monitor: refused with the frame code.
        let refusal = admit_in(&service, &call_args, &id, &frame_at(0)).unwrap_err();
        assert_eq!(admission_error_code(&refusal), "capture_frame_mismatch");
        assert!(
            refusal.to_string().contains("different monitor layout"),
            "{refusal}"
        );
        // The refused capture was not consumed: its own layout still admits it,
        // mapped through the frame's size (1920x1080 over a 4x3 screenshot).
        assert_eq!(
            admit_in(&service, &call_args, &id, &frame_at(-1920)).unwrap(),
            (960.0, 360.0)
        );
        // Admission consumed it and forgot its frame.
        assert_eq!(
            refusal_code(admit_in(&service, &call_args, &id, &frame_at(-1920))),
            "capture_not_found"
        );
        assert!(desktop_capture_frames()
            .iter()
            .all(|(recorded, _)| recorded.to_string() != id));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn any_accepted_spelling_of_an_id_finds_its_frame() {
        let _table = FRAME_TABLE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let service = CaptureService::default();
        let call_args = args("desktop-frame-spelling");
        let id = publish_in(&service, &call_args, &frame_at(0));
        let upper = format!("capture_{}", id["capture_".len()..].to_uppercase());
        assert_ne!(upper, id);

        assert_eq!(
            admit_in(&service, &call_args, &upper, &frame_at(0)).unwrap(),
            (960.0, 360.0)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recorded_desktop_captures_keep_their_lookup_codes() {
        let _table = FRAME_TABLE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let layout = frame_at(-1920);
        let changed = frame_at(0);

        // Evicted by the registry (32 captures by default) while its frame is
        // still recorded. An expired capture fails the same lookup.
        let service = CaptureService::default();
        let call_args = args("desktop-frame-evicted");
        let evicted = publish_in(&service, &call_args, &layout);
        for _ in 0..32 {
            publish_in(&service, &call_args, &layout);
        }
        assert!(desktop_capture_frames()
            .iter()
            .any(|(recorded, _)| recorded.to_string() == evicted));
        assert_eq!(
            refusal_code(admit_in(&service, &call_args, &evicted, &changed)),
            "capture_not_found"
        );

        // Retired with its session (retiring removes the capture), and
        // presented by another session.
        let service = CaptureService::default();
        let call_args = args("desktop-frame-retired");
        let retired = publish_in(&service, &call_args, &layout);
        service.retire_session(&service.binding_from_args(&call_args).unwrap());
        assert_eq!(
            refusal_code(admit_in(&service, &call_args, &retired, &changed)),
            "capture_not_found"
        );
        let foreign = publish_in(&service, &args("desktop-frame-owner"), &layout);
        assert_eq!(
            refusal_code(admit_in(&service, &call_args, &foreign, &changed)),
            "capture_generation_mismatch"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unrecorded_desktop_captures_are_refused_without_being_consumed() {
        let _table = FRAME_TABLE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let service = CaptureService::default();
        let call_args = args("desktop-frame-codes");
        let current = frame_at(0);

        assert_eq!(
            refusal_code(admit_in(&service, &call_args, "not-a-capture", &current)),
            "capture_id_invalid"
        );
        let window =
            publish_window(&service, &call_args, &png(4, 3, 0x52), 5, 6, (4, 3), (4, 3)).unwrap();
        assert_eq!(
            refusal_code(admit_in(&service, &call_args, &window, &current)),
            "capture_target_mismatch"
        );

        // A desktop capture with no recorded frame cannot be checked against
        // the current layout, so it is refused before admission consumes it.
        let unrecorded = publish_desktop(
            &service,
            &call_args,
            &png(4, 3, 0x54),
            (4, 3),
            (current.width, current.height),
        )
        .unwrap();
        let refusal = admit_in(&service, &call_args, &unrecorded, &current).unwrap_err();
        assert_eq!(admission_error_code(&refusal), "capture_frame_mismatch");
        assert!(
            refusal
                .to_string()
                .contains("no recorded Hyprland desktop frame"),
            "{refusal}"
        );
        assert!(service
            .read_for_perception(
                service.parse_capture_id(&unrecorded).unwrap(),
                &service.binding_from_args(&call_args).unwrap(),
            )
            .is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_desktop_frame_table_forgets_the_oldest_captures() {
        let _table = FRAME_TABLE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let id = |i: usize| format!("capture_{}_{i:016x}", "ab".repeat(16));
        for i in 0..(REMEMBERED_DESKTOP_CAPTURES + 5) {
            remember_desktop_frame(&id(i), frame_at(0)).unwrap();
        }
        let frames = desktop_capture_frames();
        assert_eq!(frames.len(), REMEMBERED_DESKTOP_CAPTURES);
        assert_eq!(frames.front().unwrap().0.to_string(), id(5));
        drop(frames);
        desktop_capture_frames().clear();
        assert!(remember_desktop_frame("not-a-capture", frame_at(0)).is_err());
    }

    fn png(width: u32, height: u32, value: u8) -> Vec<u8> {
        let rgba = vec![value; (width * height * 4) as usize];
        cua_driver_core::image_utils::encode_rgba_to_png(&rgba, width, height)
            .expect("encode fixture")
    }

    fn args(session: &str) -> Value {
        serde_json::json!({"_session_id": session})
    }

    fn admit_window(
        service: &CaptureService,
        args: &Value,
        capture_id: &str,
        pid: u32,
        window_id: u64,
        point: (f64, f64),
        native: (u32, u32),
    ) -> anyhow::Result<(f64, f64)> {
        admit_with_live_dimensions(
            service,
            args,
            capture_id,
            window_target(pid, window_id),
            NativeActionDimensions::new(native.0, native.1).unwrap(),
            point.0,
            point.1,
        )
    }

    fn admit_desktop(
        service: &CaptureService,
        args: &Value,
        capture_id: &str,
        point: (f64, f64),
        native: (u32, u32),
    ) -> anyhow::Result<(f64, f64)> {
        admit_with_live_dimensions(
            service,
            args,
            capture_id,
            CaptureTarget::PrimaryDesktop,
            NativeActionDimensions::new(native.0, native.1).unwrap(),
            point.0,
            point.1,
        )
    }

    #[test]
    fn window_publication_digests_the_exact_returned_png() {
        let service = CaptureService::default();
        let delivered = png(2, 2, 0x41);
        let id = publish_window(
            &service,
            &args("digest"),
            &delivered,
            41,
            99,
            (2, 2),
            (4, 4),
        )
        .expect("publish window capture");
        let binding = service.binding_from_args(&args("digest")).unwrap();
        let capture = service
            .read_for_perception(service.parse_capture_id(&id).unwrap(), &binding)
            .expect("read published capture");

        assert_eq!(capture.png_bytes().as_ref(), delivered.as_slice());
        let expected: [u8; 32] = Sha256::digest(&delivered).into();
        assert_eq!(capture.digest().as_bytes(), &expected);
    }

    #[test]
    fn target_mismatch_does_not_consume_but_successful_admission_does() {
        let service = CaptureService::default();
        let call_args = args("one-action");
        let id = publish_window(
            &service,
            &call_args,
            &png(10, 10, 0x22),
            7,
            11,
            (10, 10),
            (20, 20),
        )
        .unwrap();

        assert!(admit_window(&service, &call_args, &id, 7, 12, (2.0, 3.0), (20, 20)).is_err());
        assert_eq!(
            admit_window(&service, &call_args, &id, 7, 11, (2.0, 3.0), (20, 20)).unwrap(),
            (4.0, 6.0)
        );
        assert!(admit_window(&service, &call_args, &id, 7, 11, (2.0, 3.0), (20, 20)).is_err());
    }

    #[test]
    fn retired_session_capture_is_stale() {
        let service = CaptureService::default();
        let call_args = args("retired");
        let id = publish_desktop(&service, &call_args, &png(3, 2, 0x77), (3, 2), (3, 2)).unwrap();

        let binding = service.binding_from_args(&call_args).unwrap();
        service.retire_session(&binding);

        assert!(admit_desktop(&service, &call_args, &id, (1.0, 1.0), (3, 2)).is_err());
    }

    #[test]
    fn window_transform_preserves_each_rounded_axis() {
        let service = CaptureService::default();
        let call_args = args("rounding");
        let id = publish_window(
            &service,
            &call_args,
            &png(3, 2, 0x33),
            8,
            13,
            (3, 2),
            (5, 3),
        )
        .unwrap();

        let (x, y) = admit_window(&service, &call_args, &id, 8, 13, (1.0, 1.0), (5, 3)).unwrap();
        assert_eq!(x, 5.0 / 3.0);
        assert_eq!(y, 3.0 / 2.0);
    }

    #[test]
    fn another_session_cannot_admit_or_consume_a_capture() {
        let service = CaptureService::default();
        let owner = args("owner");
        let id = publish_desktop(&service, &owner, &png(4, 3, 0x21), (4, 3), (4, 3)).unwrap();

        let refusal = admit_desktop(&service, &args("other"), &id, (2.0, 1.0), (4, 3))
            .expect_err("cross-session admission must fail");
        assert_eq!(
            admission_error_code(&refusal),
            "capture_generation_mismatch"
        );
        assert_eq!(
            admit_desktop(&service, &owner, &id, (2.0, 1.0), (4, 3)).unwrap(),
            (2.0, 1.0)
        );
    }

    #[test]
    fn hidpi_desktop_uses_the_compositors_logical_action_frame() {
        let native = (3200, 2000);
        let logical = select_desktop_action_dimensions(native, Some((1600, 1000))).unwrap();
        assert_eq!(logical, (1600, 1000));

        let service = CaptureService::default();
        let call_args = args("hidpi");
        let id = publish_desktop(
            &service,
            &call_args,
            &png(logical.0, logical.1, 0x31),
            logical,
            logical,
        )
        .unwrap();
        assert_eq!(
            admit_desktop(&service, &call_args, &id, (800.0, 500.0), logical).unwrap(),
            (800.0, 500.0)
        );
    }

    #[test]
    fn downsized_desktop_capture_maps_back_to_the_action_frame() {
        let service = CaptureService::default();
        let call_args = args("downsized");
        let id = publish_desktop(&service, &call_args, &png(4, 3, 0x42), (4, 3), (8, 6)).unwrap();
        assert_eq!(
            admit_desktop(&service, &call_args, &id, (2.0, 1.5), (8, 6)).unwrap(),
            (4.0, 3.0)
        );
    }

    #[test]
    fn desktop_publication_uses_the_post_normalization_bytes() {
        let service = CaptureService::default();
        let normalized = png(4, 3, 0x18);
        let id = publish_desktop(&service, &args("desktop"), &normalized, (4, 3), (4, 3)).unwrap();
        let binding = service.binding_from_args(&args("desktop")).unwrap();
        let capture = service
            .read_for_perception(service.parse_capture_id(&id).unwrap(), &binding)
            .unwrap();

        assert_eq!(capture.png_bytes().as_ref(), normalized.as_slice());
        assert_eq!(
            admit_desktop(&service, &args("desktop"), &id, (2.0, 1.0), (4, 3)).unwrap(),
            (2.0, 1.0)
        );
    }

    #[test]
    fn live_resize_refuses_before_dispatch_without_consuming_capture() {
        let service = CaptureService::default();
        let call_args = args("resize");
        let id = publish_window(
            &service,
            &call_args,
            &png(4, 3, 0x44),
            8,
            13,
            (4, 3),
            (8, 6),
        )
        .unwrap();
        let mut dispatches = 0;
        let refusal = admit_window(&service, &call_args, &id, 8, 13, (1.25, 1.5), (9, 6));
        if refusal.is_ok() {
            dispatches += 1;
        }
        assert_eq!(dispatches, 0);
        assert!(refusal
            .unwrap_err()
            .downcast_ref::<cua_driver_core::capture_runtime::CaptureActionError>()
            .is_some_and(|error| {
                *error
                == cua_driver_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch
            }));
        assert_eq!(
            admit_window(&service, &call_args, &id, 8, 13, (1.25, 1.5), (8, 6),).unwrap(),
            (2.5, 3.0)
        );
    }
}
