//! Shared JS snippet builders for context-menu media/image operations.
//!
//! Each function returns a self-executing JS snippet (IIFE) as a `String`.
//! Coordinates are JSON-encoded via `serde_json` — defense in depth even
//! though they are always `i32` literals from our own code.
//!
//! Backends call these from their `BrowserEngine` trait overrides and pass the
//! result to `self.run_js(...)`.

/// Toggle `.paused` on the `<video>`/`<audio>` element under `(x, y)`.
pub fn play_pause(x: i32, y: i32) -> String {
    let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
    let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
    format!(
        "(function(x,y){{\
           var el=document.elementFromPoint(x,y);\
           while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
           if(!el)el=document.querySelector('video, audio');\
           if(!el)return;\
           if(el.paused)el.play();else el.pause();\
         }})({x},{y});"
    )
}

/// Toggle `.muted` on the media element under `(x, y)`.
pub fn toggle_mute(x: i32, y: i32) -> String {
    let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
    let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
    format!(
        "(function(x,y){{\
           var el=document.elementFromPoint(x,y);\
           while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
           if(!el)el=document.querySelector('video, audio');\
           if(!el)return;\
           el.muted=!el.muted;\
         }})({x},{y});"
    )
}

/// Toggle `.loop` on the media element under `(x, y)`.
pub fn toggle_loop(x: i32, y: i32) -> String {
    let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
    let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
    format!(
        "(function(x,y){{\
           var el=document.elementFromPoint(x,y);\
           while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
           if(!el)el=document.querySelector('video, audio');\
           if(!el)return;\
           el.loop=!el.loop;\
         }})({x},{y});"
    )
}

/// Toggle `.controls` on the media element under `(x, y)`.
pub fn toggle_controls(x: i32, y: i32) -> String {
    let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
    let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
    format!(
        "(function(x,y){{\
           var el=document.elementFromPoint(x,y);\
           while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
           if(!el)el=document.querySelector('video, audio');\
           if(!el)return;\
           el.controls=!el.controls;\
         }})({x},{y});"
    )
}

/// Request Picture-in-Picture for the media element under `(x, y)`,
/// or exit PiP if it is already the PiP element. Wrapped in try/catch
/// because some hosts disable PiP.
pub fn picture_in_picture(x: i32, y: i32) -> String {
    let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
    let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
    format!(
        "(function(x,y){{\
           try{{\
             var el=document.elementFromPoint(x,y);\
             while(el&&!(el instanceof HTMLVideoElement))el=el.parentElement;\
             if(!el)el=document.querySelector('video');\
             if(!el)return;\
             if(document.pictureInPictureElement===el){{\
               document.exitPictureInPicture();\
             }}else{{\
               el.requestPictureInPicture();\
             }}\
           }}catch(e){{}}\
         }})({x},{y});"
    )
}

/// Rotate the `<img>` element under `(x, y)` by `delta_deg` degrees.
///
/// Positive `delta_deg` = clockwise, negative = counter-clockwise.
/// Accumulates into `el.dataset.buffrRotate` so successive rotations compose.
pub fn image_rotate(x: i32, y: i32, delta_deg: i32) -> String {
    let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
    let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
    let delta = serde_json::to_string(&delta_deg).unwrap_or_else(|_| "0".to_string());
    format!(
        "(function(x,y,delta){{\
           var el=document.elementFromPoint(x,y);\
           while(el&&!(el instanceof HTMLImageElement))el=el.parentElement;\
           if(!el)return;\
           var cur=parseInt(el.dataset.buffrRotate||'0',10);\
           cur=(cur+delta)%360;\
           if(cur<0)cur+=360;\
           el.dataset.buffrRotate=String(cur);\
           el.style.transform='rotate('+cur+'deg)';\
         }})({x},{y},{delta});"
    )
}

/// Write the image URL to the clipboard via `navigator.clipboard.writeText`.
///
/// The URL is JSON-encoded to prevent injection.
pub fn copy_image_url(url: &str) -> String {
    let url = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".into());
    format!("navigator.clipboard.writeText({url});")
}
