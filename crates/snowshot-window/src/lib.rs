use std::fmt;

#[derive(Debug)]
pub enum WindowError {
    Backend(String),
    InvalidBounds,
    UnsupportedPlatform,
}

impl fmt::Display for WindowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(formatter, "window backend failed: {error}"),
            Self::InvalidBounds => write!(formatter, "window bounds are empty or invalid"),
            Self::UnsupportedPlatform => {
                write!(
                    formatter,
                    "window enumeration is not supported on this platform"
                )
            }
        }
    }
}

impl std::error::Error for WindowError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRect {
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
}

impl WindowRect {
    pub fn new(min_x: i32, min_y: i32, max_x: i32, max_y: i32) -> Result<Self, WindowError> {
        if max_x <= min_x || max_y <= min_y {
            return Err(WindowError::InvalidBounds);
        }

        Ok(Self {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }

    pub fn min_x(self) -> i32 {
        self.min_x
    }

    pub fn min_y(self) -> i32 {
        self.min_y
    }

    pub fn max_x(self) -> i32 {
        self.max_x
    }

    pub fn max_y(self) -> i32 {
        self.max_y
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowTarget {
    id: u32,
    rect: WindowRect,
}

impl WindowTarget {
    pub fn id(self) -> u32 {
        self.id
    }

    pub fn rect(self) -> WindowRect {
        self.rect
    }
}

#[cfg(target_os = "windows")]
pub fn list_windows(blacklist: &[String]) -> Result<Vec<WindowTarget>, WindowError> {
    use std::ffi::c_void;

    use rayon::prelude::*;
    use windows::Win32::Foundation::HWND;
    use xcap::{ImplWindow, Window};

    let handles = Window::all()
        .map_err(|error| WindowError::Backend(error.to_string()))?
        .into_iter()
        .filter_map(|window| window.hwnd().ok())
        .map(|handle| handle as usize)
        .collect::<Vec<_>>();
    let blacklist = normalize_blacklist(blacklist);

    Ok(handles
        .par_iter()
        .filter_map(|handle| {
            let window = ImplWindow::new(HWND(*handle as *mut c_void));
            if window.is_minimized().unwrap_or(true) {
                return None;
            }

            let title = window.title().unwrap_or_default();
            if title == "Shell Handwriting Canvas" {
                return None;
            }

            let app_name = window.app_name().unwrap_or_default();
            if is_blacklisted(&app_name, &blacklist) {
                return None;
            }

            let info = window.get_window_info().ok()?;
            let rect = WindowRect::new(
                info.rcClient.left,
                info.rcClient.top,
                info.rcClient.right,
                info.rcClient.bottom,
            )
            .ok()?;
            let id = window.id().ok()?;

            Some(WindowTarget { id, rect })
        })
        .collect())
}

#[cfg(not(target_os = "windows"))]
pub fn list_windows(_blacklist: &[String]) -> Result<Vec<WindowTarget>, WindowError> {
    Err(WindowError::UnsupportedPlatform)
}

fn normalize_blacklist(blacklist: &[String]) -> Vec<String> {
    blacklist
        .iter()
        .map(|item| item.trim().to_lowercase())
        .filter(|item| !item.is_empty())
        .collect()
}

fn is_blacklisted(app_name: &str, blacklist: &[String]) -> bool {
    let app_name = app_name.to_lowercase();
    blacklist.iter().any(|item| app_name.contains(item))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_or_inverted_bounds() {
        assert!(matches!(
            WindowRect::new(0, 0, 0, 10),
            Err(WindowError::InvalidBounds)
        ));
        assert!(matches!(
            WindowRect::new(10, 0, 5, 10),
            Err(WindowError::InvalidBounds)
        ));
    }

    #[test]
    fn keeps_negative_desktop_coordinates() {
        let rect = WindowRect::new(-1920, -100, 0, 980).unwrap();

        assert_eq!(rect.min_x(), -1920);
        assert_eq!(rect.min_y(), -100);
        assert_eq!(rect.max_x(), 0);
        assert_eq!(rect.max_y(), 980);
    }

    #[test]
    fn blacklist_matching_is_trimmed_and_case_insensitive() {
        let blacklist = normalize_blacklist(&[
            "  KeePassXC ".to_string(),
            String::new(),
            "PASSWORD".to_string(),
        ]);

        assert!(is_blacklisted("keepassxc.exe", &blacklist));
        assert!(is_blacklisted("Password Manager", &blacklist));
        assert!(!is_blacklisted("explorer.exe", &blacklist));
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn enumerates_windows_on_interactive_desktop() {
        let windows = list_windows(&[]).unwrap();

        assert!(!windows.is_empty());
    }
}
