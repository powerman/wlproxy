pub mod proto;

#[derive(Clone, Copy, Debug)]
pub enum ObjType {
    Display,
    Registry,
    XdgWmBase { ver: u32 },
    XdgSurface { ver: u32 },
    XdgToplevel { ver: u32 },
}
