//! Picture formats shared by every decoder.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneFormat {
    /// Y plane + interleaved UV plane at half resolution.
    Nv12,
    /// Y, U, V planes; chroma at half resolution.
    Yuv420p,
    /// Y, U, V planes at full resolution.
    Yuv444p,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}
