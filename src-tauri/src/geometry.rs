//! Pure geometry ported from FerriScribe; platform capture lives separately.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub fn parse_slurp_geometry(s: &str) -> Option<CaptureRect> {
    let s = s.trim();
    if let Some((pos, size)) = s.split_once(' ') {
        let (x, y) = pos.split_once(',')?;
        let (w, h) = size.split_once('x')?;
        return rect_from_parts(x, y, w, h);
    }
    if let Some((size, pos)) = s.split_once('+') {
        // ImageMagick style: WxH+X+Y
        let (w, h) = size.split_once('x')?;
        let (x, y) = pos.split_once('+')?;
        return rect_from_parts(x, y, w, h);
    }
    // Comma-separated x,y,w,h
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() == 4 {
        return rect_from_parts(parts[0], parts[1], parts[2], parts[3]);
    }
    None
}

/// Build a rect from string parts, rejecting negatives for width/height.
fn rect_from_parts(x: &str, y: &str, w: &str, h: &str) -> Option<CaptureRect> {
    let x: i32 = x.trim().parse().ok()?;
    let y: i32 = y.trim().parse().ok()?;
    let width: u32 = w.trim().parse().ok()?;
    let height: u32 = h.trim().parse().ok()?;
    // A zero-size selection is a cancel, not a capture.
    (width > 0 && height > 0).then_some(CaptureRect {
        x,
        y,
        width,
        height,
    })
}

/// Render a `CaptureRect` in the `x,y WxH` form that `slurp` prints and `grim`
/// accepts (`grim -g "x,y WxH"`). Round-trips through [`parse_slurp_geometry`].
pub fn slurp_geometry_string(rect: CaptureRect) -> String {
    format!("{},{} {}x{}", rect.x, rect.y, rect.width, rect.height)
}

/// Clamp `rect` to a `0..bounds_w` × `0..bounds_h` image, translating the
/// rect into image-local coordinates (subtracting nothing — the caller passes
/// image-relative input). Returns `None` when nothing of the rect survives
/// the intersection.
pub fn clamp_rect(rect: CaptureRect, bounds_w: i32, bounds_h: i32) -> Option<CaptureRect> {
    let left = rect.x.max(0);
    let top = rect.y.max(0);
    // Edge math in i64: a huge u32 extent cast to i32 wraps NEGATIVE (e.g.
    // u32::MAX as i32 == -1), which would make the intersection look empty.
    let right = (rect.x as i64)
        .saturating_add(rect.width as i64)
        .min(bounds_w as i64);
    let bottom = (rect.y as i64)
        .saturating_add(rect.height as i64)
        .min(bounds_h as i64);
    let width = right.saturating_sub(left as i64);
    let height = bottom.saturating_sub(top as i64);
    (width > 0 && height > 0).then_some(CaptureRect {
        x: left,
        y: top,
        width: width as u32,
        height: height as u32,
    })
}

pub fn is_png(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
}

pub const PROGRESS_WIDTH: f64 = 220.0;

pub fn indicator_position(mon_x: i32, mon_y: i32, mon_width: u32, scale: f64) -> (f64, f64) {
    let scale = if scale < 0.01 { 0.01 } else { scale };
    let phys_w = PROGRESS_WIDTH * scale;
    let logical_x = (mon_x as f64 + (mon_width as f64 - phys_w) / 2.0) / scale;
    let logical_y = mon_y as f64 / scale + 24.0;
    (logical_x, logical_y)
}

/// Union of monitor rectangles. Unlike the lineage's monitor enumeration this
/// pure helper uses i64 edges, and rejects unrepresentable desktop bounds.
pub fn monitor_union(monitors: &[CaptureRect]) -> Option<CaptureRect> {
    let first = monitors.first()?;
    let (mut left, mut top) = (i64::from(first.x), i64::from(first.y));
    let (mut right, mut bottom) = (left, top);
    for m in monitors {
        if m.width == 0 || m.height == 0 {
            return None;
        }
        left = left.min(i64::from(m.x));
        top = top.min(i64::from(m.y));
        right = right.max(i64::from(m.x) + i64::from(m.width));
        bottom = bottom.max(i64::from(m.y) + i64::from(m.height));
    }
    Some(CaptureRect {
        x: left.try_into().ok()?,
        y: top.try_into().ok()?,
        width: (right - left).try_into().ok()?,
        height: (bottom - top).try_into().ok()?,
    })
}

/// Future overlay implementations MUST visibly constrain the drag to the monitor
/// where it began. Map using THAT monitor's scale/origin, never a desktop-wide
/// overlay or selection-center monitor. Coordinates may arrive in either direction.
pub fn constrained_drag(
    monitor: CaptureRect,
    scale: f64,
    start: (f64, f64),
    end: (f64, f64),
) -> Option<CaptureRect> {
    if !scale.is_finite()
        || scale <= 0.0
        || [start.0, start.1, end.0, end.1]
            .iter()
            .any(|v| !v.is_finite())
    {
        return None;
    }
    let w = f64::from(monitor.width) / scale;
    let h = f64::from(monitor.height) / scale;
    let x1 = start.0.clamp(0.0, w);
    let x2 = end.0.clamp(0.0, w);
    let y1 = start.1.clamp(0.0, h);
    let y2 = end.1.clamp(0.0, h);
    let x = (x1.min(x2) * scale).round() as i64;
    let y = (y1.min(y2) * scale).round() as i64;
    let width = ((x1.max(x2) * scale).round() as i64 - x) as u32;
    let height = ((y1.max(y2) * scale).round() as i64 - y) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some(CaptureRect {
        x: (i64::from(monitor.x) + x).try_into().ok()?,
        y: (i64::from(monitor.y) + y).try_into().ok()?,
        width,
        height,
    })
}

/// Map a desktop-coordinate selection (physical pixels, e.g. from a Win32
/// overlay) to the monitor where it began, returning the monitor's index and
/// the selection translated into monitor-LOCAL pixels for cropping.
///
/// Doctrine (see [`constrained_drag`]): the drag is constrained to its origin
/// monitor, so a selection that is not FULLY inside a single monitor means a
/// clamp failed upstream — reject it rather than silently truncating a
/// cross-monitor crop. Zero-size selections are rejected the same way
/// (a click without a drag is a cancel, not a capture).
pub fn monitor_local_crop(
    monitors: &[CaptureRect],
    selection: CaptureRect,
) -> Option<(usize, CaptureRect)> {
    // Zero-size is a cancel (a click without a drag), not a capture.
    if selection.width == 0 || selection.height == 0 {
        return None;
    }
    let (idx, monitor) = monitors.iter().enumerate().find(|(_, m)| {
        // Origin monitor = the monitor containing the selection's origin.
        selection.x >= m.x
            && selection.y >= m.y
            && selection.x < m.x.saturating_add(m.width as i32)
            && selection.y < m.y.saturating_add(m.height as i32)
    })?;
    let local = CaptureRect {
        x: selection.x.checked_sub(monitor.x)?,
        y: selection.y.checked_sub(monitor.y)?,
        width: selection.width,
        height: selection.height,
    };
    // Fully-inside check: any spill outside the origin monitor is a bug in
    // the overlay's clamp — reject rather than truncate (crop would fail
    // anyway; failing HERE keeps the error at the mapping seam with tests).
    let inside = local.x >= 0
        && local.y >= 0
        && i64::from(local.x) + i64::from(local.width) <= i64::from(monitor.width)
        && i64::from(local.y) + i64::from(local.height) <= i64::from(monitor.height);
    if !inside {
        return None;
    }
    Some((idx, local))
}

/// The image half of capture_screen_region_png, ready for future platform
/// CaptureDevice implementations. Reject partial crops: never silently truncate a
/// cross-monitor drag. Preserve the lineage's alpha-force and in-memory PNG.
pub fn capture_screen_region_png(
    image: &image::RgbaImage,
    local: CaptureRect,
) -> Result<Vec<u8>, crate::core::Failure> {
    let bounds_w = i32::try_from(image.width()).map_err(|_| crate::core::Failure::Capture)?;
    let bounds_h = i32::try_from(image.height()).map_err(|_| crate::core::Failure::Capture)?;
    if clamp_rect(local, bounds_w, bounds_h) != Some(local) {
        return Err(crate::core::Failure::Capture);
    }
    use image::GenericImageView;
    let mut cropped = image
        .view(local.x as u32, local.y as u32, local.width, local.height)
        .to_image();
    for pixel in cropped.pixels_mut() {
        pixel.0[3] = 255;
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(cropped)
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|_| crate::core::Failure::Capture)?;
    Ok(png.into_inner())
}

/// Encode a tightly-packed RGBA8 buffer as an opaque PNG. Used by the Windows
/// capture path (WinRT frames arrive as raw RGBA8). Rejects a byte count that
/// does not match `width * height * 4` and forces alpha to 255, mirroring
/// [`capture_screen_region_png`].
pub fn rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, crate::core::Failure> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > crate::core::MAX_PIXELS {
        return Err(crate::core::Failure::Capture);
    }
    let expected = (u64::from(width) * u64::from(height) * 4) as usize;
    if rgba.len() != expected {
        return Err(crate::core::Failure::Capture);
    }
    let mut image = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or(crate::core::Failure::Capture)?;
    for pixel in image.pixels_mut() {
        pixel.0[3] = 255;
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|_| crate::core::Failure::Capture)?;
    Ok(png.into_inner())
}

/// Platform selector boundary reserved for per-monitor overlays. The macOS
/// CaptureDevice uses the system picker, and Linux/Windows currently fail with an
/// explicit unsupported outcome rather than a silently wrong union overlay.
#[async_trait::async_trait]
pub trait OverlaySelector: Send + Sync {
    async fn select(
        &self,
        monitors: &[CaptureRect],
    ) -> Result<Option<CaptureRect>, crate::core::Failure>;
}
pub struct UnsupportedOverlay;
#[async_trait::async_trait]
impl OverlaySelector for UnsupportedOverlay {
    async fn select(&self, _: &[CaptureRect]) -> Result<Option<CaptureRect>, crate::core::Failure> {
        Err(crate::core::Failure::Unsupported)
    }
}

#[cfg(test)]
mod hardened_tests {
    use super::*;
    #[test]
    fn union_handles_negative_origins_and_huge_extents() {
        let left = CaptureRect {
            x: -2560,
            y: -200,
            width: 2560,
            height: 1440,
        };
        let right = CaptureRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            monitor_union(&[left, right]),
            Some(CaptureRect {
                x: -2560,
                y: -200,
                width: 4480,
                height: 1440
            })
        );
        assert_eq!(monitor_union(&[]), None);
        assert!(monitor_union(&[
            CaptureRect {
                x: i32::MIN,
                y: 0,
                width: 1,
                height: 1
            },
            CaptureRect {
                x: i32::MAX,
                y: 0,
                width: u32::MAX,
                height: 1
            }
        ])
        .is_none());
    }
    #[test]
    fn mixed_dpi_cross_monitor_drags_are_constrained_in_both_directions() {
        let left = CaptureRect {
            x: -2560,
            y: -200,
            width: 2560,
            height: 1440,
        };
        let expected = Some(CaptureRect {
            x: -2560,
            y: -180,
            width: 2560,
            height: 180,
        });
        assert_eq!(
            constrained_drag(left, 2.0, (-100.0, 10.0), (1400.0, 100.0)),
            expected
        );
        assert_eq!(
            constrained_drag(left, 2.0, (1400.0, 100.0), (-100.0, 10.0)),
            expected
        );
        let right = CaptureRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            constrained_drag(right, 1.0, (-500.0, 10.0), (500.0, 100.0)),
            Some(CaptureRect {
                x: 0,
                y: 10,
                width: 500,
                height: 90
            })
        );
        assert!(constrained_drag(right, f64::NAN, (0.0, 0.0), (1.0, 1.0)).is_none());
    }
    #[test]
    fn image_crop_forces_alpha_and_rejects_partial_capture() {
        let image = image::RgbaImage::from_pixel(10, 10, image::Rgba([12, 34, 56, 0]));
        let png = capture_screen_region_png(
            &image,
            CaptureRect {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
        )
        .unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (3, 4));
        assert!(decoded.pixels().all(|p| p.0 == [12, 34, 56, 255]));
        assert!(capture_screen_region_png(
            &image,
            CaptureRect {
                x: 9,
                y: 2,
                width: 3,
                height: 4
            }
        )
        .is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slurp_default_geometry() {
        assert_eq!(
            parse_slurp_geometry("10,20 300x400"),
            Some(CaptureRect {
                x: 10,
                y: 20,
                width: 300,
                height: 400
            })
        );
        // Tolerate surrounding whitespace/newline.
        assert_eq!(
            parse_slurp_geometry("  0,0 1920x1080\n"),
            Some(CaptureRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
    }

    #[test]
    fn parses_alternate_geometry_formats() {
        assert_eq!(
            parse_slurp_geometry("300x400+10+20"),
            Some(CaptureRect {
                x: 10,
                y: 20,
                width: 300,
                height: 400
            })
        );
        assert_eq!(
            parse_slurp_geometry("10,20,300,400"),
            Some(CaptureRect {
                x: 10,
                y: 20,
                width: 300,
                height: 400
            })
        );
    }

    #[test]
    fn rejects_malformed_or_degenerate_geometry() {
        // Empty output = user cancelled / garbage.
        for bad in [
            "",
            "   ",
            "10,20",
            "a,b 1x1",
            "10,20 0x400",
            "10,20 300x0",
            "10,20 -3x4",
        ] {
            assert_eq!(parse_slurp_geometry(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn accepts_negative_origins() {
        // Monitors arranged left of the primary have negative coordinates.
        assert_eq!(
            parse_slurp_geometry("-1920,0 1920x1080"),
            Some(CaptureRect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
    }

    #[test]
    fn clamp_rect_intersects_and_translates() {
        let full = CaptureRect {
            x: 10,
            y: 10,
            width: 100,
            height: 100,
        };
        // Fully inside → unchanged.
        assert_eq!(
            clamp_rect(full, 1920, 1080),
            Some(CaptureRect {
                x: 10,
                y: 10,
                width: 100,
                height: 100
            })
        );
        // Hanging off the right/bottom edge → truncated.
        assert_eq!(
            clamp_rect(
                CaptureRect {
                    x: 1900,
                    y: 1000,
                    width: 100,
                    height: 100
                },
                1920,
                1080
            ),
            Some(CaptureRect {
                x: 1900,
                y: 1000,
                width: 20,
                height: 80
            })
        );
        // Negative origin → clamped to 0 with size reduced accordingly.
        assert_eq!(
            clamp_rect(
                CaptureRect {
                    x: -10,
                    y: -10,
                    width: 100,
                    height: 100
                },
                1920,
                1080
            ),
            Some(CaptureRect {
                x: 0,
                y: 0,
                width: 90,
                height: 90
            })
        );
        // Completely outside → None (nothing to OCR).
        assert_eq!(
            clamp_rect(
                CaptureRect {
                    x: 2000,
                    y: 0,
                    width: 10,
                    height: 10
                },
                1920,
                1080
            ),
            None
        );
        // Zero-size after clamp → None.
        assert_eq!(
            clamp_rect(
                CaptureRect {
                    x: 1920,
                    y: 0,
                    width: 10,
                    height: 10
                },
                1920,
                1080
            ),
            None
        );
    }

    #[test]
    fn clamp_rect_saturates_on_huge_values() {
        // i32::MAX width must not overflow when added to x.
        let rect = CaptureRect {
            x: 0,
            y: 0,
            width: u32::MAX,
            height: u32::MAX,
        };
        assert_eq!(
            clamp_rect(rect, 100, 100),
            Some(CaptureRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100
            })
        );
    }

    #[test]
    fn monitor_local_crop_translates_negative_origin_monitor() {
        // Monitor left of primary at negative origin, mixed-DPI arrangement.
        let monitors = [
            CaptureRect {
                x: -2560,
                y: -200,
                width: 2560,
                height: 1440,
            },
            CaptureRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        // Selection fully inside the left monitor → index + local coords.
        assert_eq!(
            monitor_local_crop(
                &monitors,
                CaptureRect {
                    x: -2560,
                    y: -180,
                    width: 2560,
                    height: 180,
                }
            ),
            Some((
                0,
                CaptureRect {
                    x: 0,
                    y: 20,
                    width: 2560,
                    height: 180,
                }
            ))
        );
        // Selection in the right monitor maps to index 1, origin-relative.
        assert_eq!(
            monitor_local_crop(
                &monitors,
                CaptureRect {
                    x: 10,
                    y: 10,
                    width: 500,
                    height: 90,
                }
            ),
            Some((
                1,
                CaptureRect {
                    x: 10,
                    y: 10,
                    width: 500,
                    height: 90,
                }
            ))
        );
    }

    #[test]
    fn monitor_local_crop_rejects_cross_monitor_spill() {
        // A selection that hangs off its origin monitor means the overlay's
        // clamp failed — reject, never truncate.
        let monitors = [CaptureRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        assert_eq!(
            monitor_local_crop(
                &monitors,
                CaptureRect {
                    x: 1000,
                    y: 0,
                    width: 1000,
                    height: 100,
                }
            ),
            None
        );
        assert_eq!(
            monitor_local_crop(
                &monitors,
                CaptureRect {
                    x: 0,
                    y: 1000,
                    width: 100,
                    height: 200,
                }
            ),
            None
        );
        // Origin in the gap / outside every monitor → None.
        assert_eq!(
            monitor_local_crop(
                &monitors,
                CaptureRect {
                    x: 5000,
                    y: 5000,
                    width: 10,
                    height: 10,
                }
            ),
            None
        );
    }

    #[test]
    fn monitor_local_crop_rejects_zero_size_selection() {
        // A click without a drag: zero-size is a cancel, not a crop.
        let monitors = [CaptureRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        assert_eq!(
            monitor_local_crop(
                &monitors,
                CaptureRect {
                    x: 100,
                    y: 100,
                    width: 0,
                    height: 50,
                }
            ),
            None
        );
        assert_eq!(
            monitor_local_crop(
                &monitors,
                CaptureRect {
                    x: 100,
                    y: 100,
                    width: 50,
                    height: 0,
                }
            ),
            None
        );
    }

    #[test]
    fn drag_clamped_at_monitor_edge_crops_exactly_to_the_edge() {
        // End-to-end mapping: a drag that runs past the monitor edge is
        // clamped by constrained_drag, and the clamped selection maps to a
        // local crop that reaches exactly to the monitor edge.
        let monitor = CaptureRect {
            x: -2560,
            y: -200,
            width: 2560,
            height: 1440,
        };
        // Logical drag from inside to far past the right/bottom edge.
        let clamped = constrained_drag(monitor, 2.0, (10.0, 10.0), (2000.0, 1000.0))
            .expect("clamped drag survives");
        assert_eq!(
            clamped,
            CaptureRect {
                x: -2540,
                y: -180,
                width: 2540,
                height: 1440 - 20,
            }
        );
        let (idx, local) = monitor_local_crop(&[monitor], clamped).expect("maps cleanly");
        assert_eq!(idx, 0);
        assert_eq!(
            local,
            CaptureRect {
                x: 20,
                y: 20,
                width: 2540,
                height: 1420,
            }
        );
        // A drag entirely outside the monitor clamps to nothing (cancel).
        assert_eq!(
            constrained_drag(monitor, 2.0, (-5.0, -5.0), (-1.0, -1.0)),
            None
        );
    }

    #[test]
    fn sniffs_png_signature() {
        assert!(is_png(&[
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0
        ]));
        assert!(!is_png(b"PNGPNGPNG"));
        assert!(!is_png(&[0x89, b'P', b'N', b'G']));
        assert!(!is_png(b""));
    }

    #[test]
    fn indicator_position_centers_on_monitor_top() {
        // 1920-wide monitor at origin, scale 1: pill centered, 24px down.
        let (x, y) = indicator_position(0, 0, 1920, 1.0);
        assert!((x - (1920.0 - PROGRESS_WIDTH) / 2.0).abs() < 0.01);
        assert!((y - 24.0).abs() < 0.01);
    }

    #[test]
    fn indicator_position_handles_scaled_secondary_monitor() {
        // Left-of-primary monitor at x=-2560, 2x scale (physical coords in,
        // logical position out).
        let (x, y) = indicator_position(-2560, 0, 2560, 2.0);
        // Physical pill width 440 → centered: (-2560 + (2560-440)/2)/2.
        assert!((x - (-2560.0 + (2560.0 - 440.0) / 2.0) / 2.0).abs() < 0.01);
        assert!((y - 24.0).abs() < 0.01);
    }

    #[test]
    fn indicator_position_never_divides_by_zero() {
        let (x, y) = indicator_position(0, 0, 1920, 0.0);
        assert!(x.is_finite() && y.is_finite());
    }

    #[test]
    fn slurp_geometry_string_roundtrips_through_parser() {
        for rect in [
            CaptureRect {
                x: 10,
                y: 20,
                width: 300,
                height: 400,
            },
            CaptureRect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ] {
            let s = slurp_geometry_string(rect);
            assert_eq!(parse_slurp_geometry(&s), Some(rect), "roundtrip {s:?}");
        }
    }

    #[test]
    fn rgba_to_png_forces_opaque_alpha_and_rejects_bad_sizes() {
        let data = vec![12u8, 34, 56, 0, 200, 201, 202, 0];
        let png = rgba_to_png(2, 1, &data).unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.pixels().collect::<Vec<_>>()[0].0, [12, 34, 56, 255]);
        assert_eq!(
            decoded.pixels().collect::<Vec<_>>()[1].0,
            [200, 201, 202, 255]
        );
        // Wrong byte count for the declared dimensions is rejected.
        assert!(rgba_to_png(2, 1, &data[..4]).is_err());
        assert!(rgba_to_png(0, 1, &data).is_err());
        assert!(rgba_to_png(1, 0, &data).is_err());
    }
}
