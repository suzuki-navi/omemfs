pub mod color;
pub mod escape;
pub mod output;

pub use color::{ColorChoice, Styles, color_enabled};
pub use escape::{escape_display, escape_display_multiline};
pub use output::{Output, paint};
