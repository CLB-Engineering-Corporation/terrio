//! Rewrite an existing scientific snapshot through terrio, retaining raw packed values.
//! Usage: cargo run -p terrio --example repack_netcdf -- source.nc destination.nc
fn main() -> terrio::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        return Err(terrio::Error::InvalidArgument("expected source.nc destination.nc".into()));
    }
    let source = terrio::multidim::open(&args[0])?;
    source.write_netcdf(&args[1], &terrio::multidim::NetcdfWriteOptions { deflate: Some(4) })
}
