//! Native NFS backend for the `nfs4fs` Python distribution.

use pyo3::prelude::*;

#[pymodule(gil_used = false)]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    vfsi_python_native::register(m, env!("CARGO_PKG_VERSION"))
}
