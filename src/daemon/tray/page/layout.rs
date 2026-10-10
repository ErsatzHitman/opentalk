//! Page layout (renderer agent A). Pure: `layout(items, measure, width)` places the page's
//! [`Item`](super::Item)s into rects (`Placed { rect, item_idx, hit rects }`) and returns the
//! content height. `measure` is injected (`&dyn Fn(&str, f32 /* px */) -> f32`) so tests need no fonts.
//!
//! Heights (logical px): toggle row 44 (+15.5 per extra description line), Choice 44, stacked
//! Field 62 (Multiline: rows * 17 + 28), Segmented 34, Button 34, Radio 52, `Row` = `Row::height`.
//! Also owns focus order (interactive items only) and the virtual-list index math.
//!
//! STUB: nothing here yet.
