fn main() {
    uniffi::generate_scaffolding("./src/sdr_remote.udl").unwrap();
}
