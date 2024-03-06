// Copyright 2023, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! High-level FDT functions.

use core::ops::Range;
use cstr::cstr;
use libfdt::{Fdt, FdtError};

/// Reads the DICE data range from the given `fdt`.
pub fn read_dice_range_from(fdt: &Fdt) -> libfdt::Result<Range<usize>> {
    let node = fdt.node(cstr!("/reserved-memory"))?.ok_or(FdtError::NotFound)?;
    let node = node.next_compatible(cstr!("google,open-dice"))?.ok_or(FdtError::NotFound)?;
    node.first_reg()?.try_into()
}
