#![allow(unused_imports)]

mod auto;
mod login;
mod misc;
pub(crate) mod profile;
mod render;
mod update;

pub(crate) use crate::launch::{launch_cmd, launch_for_tui};
pub(crate) use auto::{AutoOptions, auto_cmd};
pub(crate) use login::login_cmd;
pub(crate) use misc::{doctor_cmd, format_resync_confirm_prompt, open_cmd};
pub(crate) use profile::{restore_cmd, delete_cmd, list_cmd, rename_cmd, use_cmd};
pub(crate) use render::confirm;
pub(crate) use update::self_update_cmd;
