// Copyright (c) 2024-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use path_absolutize::Absolutize;
use std::path::{Path, PathBuf};

#[allow(clippy::module_name_repetitions)]
pub fn absolute_path<P: AsRef<Path>>(path: P) -> PathBuf {
    // TODO: std::path::absolute 1.79
    path.as_ref()
        .absolutize()
        .expect("should be absolute path")
        .into()
}
