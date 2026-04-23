// SPDX-License-Identifier: GPL-2.0-or-later

fn main() {
    uniffi::generate_scaffolding("./src/sdr_remote.udl").unwrap();
}
