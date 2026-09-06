//! Image handling shared by every input path (clipboard image flavor, file
//! list, HTML-embedded references): deciding which images are worth sending.

pub mod encode;
pub mod filter;
pub mod html;
