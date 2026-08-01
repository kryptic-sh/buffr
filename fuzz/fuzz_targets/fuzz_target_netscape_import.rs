#![no_main]

//! Fuzz the Netscape bookmark-file importer.
//!
//! `Bookmarks::import_netscape` is a hand-rolled regex walker
//! (`(?is)<H3[^>]*>(.*?)</H3>` and friends) over a file the user picks
//! but does not author — a bookmark export handed over by a third party
//! is fully attacker-controlled. Lazy `.*?` over adversarial input plus
//! the byte-offset token sort and the folder-stack bookkeeping are all
//! worth exercising; the store is in-memory so a run leaves no state.

use libfuzzer_sys::fuzz_target;

use buffr_bookmarks::Bookmarks;

fuzz_target!(|data: &[u8]| {
    let Ok(html) = std::str::from_utf8(data) else {
        return;
    };
    // Fresh store per input: import is one transaction, and reusing a
    // store across inputs would make crashes depend on iteration order.
    let Ok(store) = Bookmarks::open_in_memory() else {
        return;
    };
    let _ = store.import_netscape(html);
});
