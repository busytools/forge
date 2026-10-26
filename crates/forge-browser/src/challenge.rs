//! A page's CAPTCHA frames and whether any of them is live.
//!
//! **Presence is not liveness.** Google's EXPIRED reCAPTCHA overlay is an
//! iframe parked at `top: -10000px` with `visibility: hidden`, and its rect is
//! still a real rect at viewport y = -13576, so presence and geometry both
//! read it as a live widget. The page answers the signals itself, where the
//! browser's own layout and hit testing are the truth.

use serde_json::{Value, json};

/// The vendors whose frames count as a challenge, matched case-insensitively
/// against an iframe's `src` and `title`.
///
/// **One list, and the page script is generated from it**: a vendor is added
/// here and nowhere else.
pub(crate) const VENDORS: &[(&str, &[&str])] = &[
    ("reCAPTCHA", &["recaptcha"]),
    ("hCaptcha", &["hcaptcha"]),
    ("Turnstile", &["turnstile", "cloudflare"]),
];

/// The page script, with [`VENDORS`] substituted in.
pub(crate) fn state_js() -> String {
    let vendors: Vec<Value> =
        VENDORS.iter().map(|(name, markers)| json!({ "name": name, "markers": markers })).collect();
    STATE_JS.replace("__VENDORS__", &Value::Array(vendors).to_string())
}

/// The script the page runs. One string, so what a test drives is what the
/// browser runs.
const STATE_JS: &str = r#"() => {
  const VENDORS = __VENDORS__;
  const viewport = { width: window.innerWidth, height: window.innerHeight };
  const challenges = [];
  for (const frame of document.querySelectorAll("iframe")) {
    const named = `${frame.getAttribute("src") || ""} ${frame.getAttribute("title") || ""}`.toLowerCase();
    const vendor = VENDORS.find((entry) => entry.markers.some((marker) => named.includes(marker)));
    if (!vendor) continue;
    const rect = frame.getBoundingClientRect();
    const style = getComputedStyle(frame);
    const opacity = Number(style.opacity);
    const visible = style.display !== "none" && style.visibility !== "hidden" && style.visibility !== "collapse" && !(opacity <= 0);
    const onScreen = rect.width > 0 && rect.height > 0 && rect.left < viewport.width && rect.top < viewport.height && rect.right > 0 && rect.bottom > 0;
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    const top = onScreen ? document.elementFromPoint(x, y) : null;
    const hitTestable = !!top && (top === frame || frame.contains(top));
    challenges.push({
      present: true,
      vendor: vendor.name,
      title: frame.getAttribute("title") || "",
      src: (frame.getAttribute("src") || "").slice(0, 300),
      visible,
      onScreen,
      hitTestable,
      live: visible && onScreen && hitTestable,
      rect: { x: rect.left, y: rect.top, width: rect.width, height: rect.height },
      style: { display: style.display, visibility: style.visibility, opacity: style.opacity }
    });
  }
  return { viewport, challengePresent: challenges.length > 0, challenges };
}"#;
