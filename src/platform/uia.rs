//! Selection read through UI Automation (ADR-0003). Fallback only: controls
//! that expose no selection, or an empty one, say nothing about the user's.

use std::time::{Duration, Instant};

use tracing::debug;
use windows::core::Interface;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationTreeWalker, UIA_DocumentControlTypeId, UIA_TextPatternId,
};

/// Limits on one cross-process call; the client defaults are 2 s and 20 s.
const CONNECTION_TIMEOUT_MS: u32 = 300;
const TRANSACTION_TIMEOUT_MS: u32 = 500;
/// After this long no further call is started; one already in flight still
/// runs to its transaction timeout.
const LOOKUP_BUDGET: Duration = Duration::from_millis(800);
/// Focus can sit on a container while an ancestor owns the selection.
const MAX_ANCESTORS: usize = 10;
/// Most ranges a selection may have (a block selection has one per line);
/// a larger one is not read rather than returned cut short.
const MAX_RANGES: i32 = 256;

/// What the selection search needs from an accessibility element.
trait Node: Sized {
    fn process_id(&self) -> Option<u32>;
    /// `None` when the property cannot be read.
    fn is_password(&self) -> Option<bool>;
    /// Root of a page, frame or editor; `None` when the type cannot be read.
    fn is_document(&self) -> Option<bool>;
    /// Selected text when the element exposes a text pattern; may be blank.
    fn selection(&self, deadline: Instant) -> Option<String>;
    fn parent(&self) -> Option<Self>;
}

fn expired(deadline: Instant) -> bool {
    Instant::now() > deadline
}

/// The selection of `focused`, or of the nearest ancestor that has one, up
/// to and including the enclosing document. A property that cannot be read
/// ends the search: not knowing is not permission.
fn find_selection<N: Node>(focused: N, own_pid: u32, deadline: Instant) -> Option<String> {
    if expired(deadline) {
        return None;
    }
    // The overlay itself holds no user selection.
    if focused.process_id()? == own_pid {
        return None;
    }
    let mut node = focused;
    for depth in 0..=MAX_ANCESTORS {
        if expired(deadline) {
            debug!("UI Automation lookup exceeded its budget");
            return None;
        }
        // A password field answers with mask characters, and the copy it
        // refused is not to be worked around.
        if node.is_password()? {
            debug!("password field on the focus path; selection not read");
            return None;
        }
        let text = node.selection(deadline).map(|text| normalize(&text));
        if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
            return Some(text);
        }
        // A frame's or editor's selection is its own - an outer page's
        // earlier selection is not what a copy from here would have taken.
        if depth == MAX_ANCESTORS
            || expired(deadline)
            || node.is_document()?
            || expired(deadline)
        {
            debug!("no selection up to the enclosing document (depth {depth})");
            return None;
        }
        let Some(parent) = node.parent() else {
            debug!("no selection up to the root (depth {depth})");
            return None;
        };
        node = parent;
    }
    None
}

/// Fold UI Automation text into the clipboard path's shape: RichEdit
/// separates paragraphs with a bare `\r`, and embedded objects appear as
/// U+FFFC, which a native copy omits.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n").replace('\u{fffc}', "")
}

struct UiaNode<'a> {
    element: IUIAutomationElement,
    walker: &'a IUIAutomationTreeWalker,
}

impl Node for UiaNode<'_> {
    fn process_id(&self) -> Option<u32> {
        unsafe { self.element.CurrentProcessId() }
            .inspect_err(|e| debug!("ProcessId unreadable: {e}"))
            .ok()
            .map(|pid| pid as u32)
    }

    fn is_password(&self) -> Option<bool> {
        unsafe { self.element.CurrentIsPassword() }
            .inspect_err(|e| debug!("IsPassword unreadable: {e}"))
            .ok()
            .map(|protected| protected.as_bool())
    }

    fn is_document(&self) -> Option<bool> {
        unsafe { self.element.CurrentControlType() }
            .inspect_err(|e| debug!("ControlType unreadable: {e}"))
            .ok()
            .map(|kind| kind == UIA_DocumentControlTypeId)
    }

    fn selection(&self, deadline: Instant) -> Option<String> {
        if expired(deadline) {
            return None;
        }
        let pattern: IUIAutomationTextPattern =
            unsafe { self.element.GetCurrentPatternAs(UIA_TextPatternId) }.ok()?;
        if expired(deadline) {
            return None;
        }
        let ranges = unsafe { pattern.GetSelection() }
            .inspect_err(|e| debug!("GetSelection failed: {e}"))
            .ok()?;
        if expired(deadline) {
            return None;
        }
        let count = unsafe { ranges.Length() }.ok()?;
        if count > MAX_RANGES {
            debug!("selection has {count} ranges; not read");
            return None;
        }
        let mut parts = Vec::new();
        for index in 0..count {
            if expired(deadline) {
                return None;
            }
            let range = unsafe { ranges.GetElement(index) }.ok()?;
            if expired(deadline) {
                return None;
            }
            parts.push(unsafe { range.GetText(-1) }.ok()?.to_string());
        }
        Some(parts.join("\n"))
    }

    fn parent(&self) -> Option<Self> {
        let element = unsafe { self.walker.GetParentElement(&self.element) }.ok()?;
        Some(Self { element, walker: self.walker })
    }
}

/// Balances a successful `CoInitializeEx` on drop.
struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

/// The focused control's selection, or the nearest ancestor's within the
/// same document; never a password field's. Call from a thread that owns no
/// window. Each cross-process call is limited by the transaction timeout and
/// none starts once the lookup budget is spent.
pub(super) fn selected_text() -> Option<String> {
    let deadline = Instant::now() + LOOKUP_BUDGET;
    // Declared first so it drops after every interface pointer. A failed
    // initialization (thread already in another apartment) leaves COM usable
    // and must not be balanced.
    let _apartment =
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok().then(|| ComApartment);
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }.ok()?;
    if let Ok(v2) = automation.cast::<IUIAutomation2>() {
        unsafe {
            let _ = v2.SetConnectionTimeout(CONNECTION_TIMEOUT_MS);
            let _ = v2.SetTransactionTimeout(TRANSACTION_TIMEOUT_MS);
        }
    }
    let walker = unsafe { automation.ControlViewWalker() }.ok()?;
    let element = unsafe { automation.GetFocusedElement() }
        .inspect_err(|e| debug!("no focused element: {e}"))
        .ok()?;
    find_selection(UiaNode { element, walker: &walker }, std::process::id(), deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN_PID: u32 = 100;
    const OTHER_PID: u32 = 200;

    #[derive(Clone)]
    struct FakeNode {
        pid: Option<u32>,
        password: Option<bool>,
        document: Option<bool>,
        selection: Option<&'static str>,
        parent: Option<Box<FakeNode>>,
    }

    impl Node for FakeNode {
        fn process_id(&self) -> Option<u32> {
            self.pid
        }
        fn is_password(&self) -> Option<bool> {
            self.password
        }
        fn is_document(&self) -> Option<bool> {
            self.document
        }
        fn selection(&self, _deadline: Instant) -> Option<String> {
            self.selection.map(str::to_owned)
        }
        fn parent(&self) -> Option<Self> {
            self.parent.as_deref().cloned()
        }
    }

    fn node(selection: Option<&'static str>) -> FakeNode {
        FakeNode {
            pid: Some(OTHER_PID),
            password: Some(false),
            document: Some(false),
            selection,
            parent: None,
        }
    }

    fn under(mut child: FakeNode, parent: FakeNode) -> FakeNode {
        child.parent = Some(Box::new(parent));
        child
    }

    fn search(focused: FakeNode) -> Option<String> {
        find_selection(focused, OWN_PID, Instant::now() + Duration::from_secs(60))
    }

    #[test]
    fn normalize_folds_line_separators_to_lf() {
        assert_eq!(normalize("one\rtwo\r"), "one\ntwo\n");
        assert_eq!(normalize("one\r\ntwo"), "one\ntwo");
        assert_eq!(normalize("one\ntwo"), "one\ntwo");
    }

    #[test]
    fn normalize_drops_embedded_object_placeholders() {
        assert_eq!(normalize("\u{fffc}Welcome \u{fffc}back"), "Welcome back");
    }

    #[test]
    fn the_focused_elements_selection_is_returned_normalized() {
        assert_eq!(search(node(Some("one\rtwo"))).as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn a_blank_or_missing_selection_defers_to_the_nearest_ancestor() {
        let page = FakeNode { document: Some(true), ..node(Some("page selection")) };
        assert_eq!(search(under(node(None), page.clone())).as_deref(), Some("page selection"));
        assert_eq!(search(under(node(Some(" \n")), page)).as_deref(), Some("page selection"));
    }

    #[test]
    fn nothing_selected_anywhere_is_none() {
        assert_eq!(search(under(node(Some("")), node(None))), None);
    }

    #[test]
    fn the_overlays_own_elements_are_ignored() {
        let own = FakeNode { pid: Some(OWN_PID), ..node(Some("overlay text")) };
        assert_eq!(search(own), None);
        let unknown = FakeNode { pid: None, ..node(Some("text")) };
        assert_eq!(search(unknown), None);
    }

    #[test]
    fn a_password_field_is_never_read() {
        // Its text pattern answers with mask characters.
        let field = FakeNode { password: Some(true), ..node(Some("********")) };
        assert_eq!(search(field.clone()), None);
        let blank = FakeNode { selection: None, ..field };
        assert_eq!(search(under(blank, node(Some("page selection")))), None);
    }

    #[test]
    fn a_password_field_above_the_focus_is_not_read_either() {
        let field = FakeNode { password: Some(true), ..node(Some("********")) };
        assert_eq!(search(under(node(None), field)), None);
    }

    #[test]
    fn an_unreadable_property_ends_the_search() {
        // Not knowing whether it is a password field is not permission.
        let unknown = FakeNode { password: None, ..node(Some("maybe a password")) };
        assert_eq!(search(unknown), None);
        // Not knowing whether a document boundary was reached, either.
        let outer = node(Some("outer page selection"));
        let frame = FakeNode { document: None, ..node(Some("")) };
        assert_eq!(search(under(frame, outer)), None);
    }

    #[test]
    fn the_search_stops_at_the_enclosing_document() {
        // Caret in an input inside a frame: the outer page's old selection is
        // not what a copy from the frame would have taken.
        let outer = FakeNode { document: Some(true), ..node(Some("outer page selection")) };
        let frame = under(FakeNode { document: Some(true), ..node(Some("")) }, outer);
        assert_eq!(search(under(node(Some("")), frame)), None);
    }

    #[test]
    fn the_search_gives_up_past_the_ancestor_limit() {
        let mut chain = node(Some("too far up"));
        for _ in 0..=MAX_ANCESTORS {
            chain = under(node(None), chain);
        }
        assert_eq!(search(chain), None);

        let mut chain = node(Some("just in reach"));
        for _ in 0..MAX_ANCESTORS {
            chain = under(node(None), chain);
        }
        assert_eq!(search(chain).as_deref(), Some("just in reach"));
    }

    #[test]
    fn an_exhausted_budget_ends_the_search() {
        let spent = Instant::now() - Duration::from_millis(1);
        assert_eq!(find_selection(node(Some("selection")), OWN_PID, spent), None);
    }
}
