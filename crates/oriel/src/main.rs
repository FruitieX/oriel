fn main() {
    oriel_media::init_logging();
    println!("{:?}", oriel_media::available_encoders());
}
