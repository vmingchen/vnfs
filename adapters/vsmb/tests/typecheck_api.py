"""Static public-API smoke test; mypy checks this file in CI."""

from vsmb import SmbClient, _native


def use_api(client: SmbClient) -> None:
    stats, errors = client.stat_many(["/a", "/b"])
    assert len(stats) == 2
    assert isinstance(errors, dict)
    data, read_errors = client.read_many(["/a"], [0], [4])
    assert data[0] in (None, b"data")
    assert isinstance(read_errors, dict)
    client.write_many(["/out"], [b"data"])
    for outcome in client.read_allv(["/a"], max_total_bytes=4):
        if isinstance(outcome, bytes):
            assert len(outcome) <= 4
        else:
            assert isinstance(outcome, (OSError, NotImplementedError))
    for positioned in client.preadv([1], [0], [4]):
        if isinstance(positioned, bytes):
            assert len(positioned) <= 4
    for written in client.pwritev([1], [0], [b"data"]):
        if isinstance(written, int):
            assert written <= 4
    for appended in client.appendv([1], [b"data"]):
        if isinstance(appended, tuple):
            assert appended[0] <= 4
    client.read_all_many(["/a"], max_total_bytes=4)
    client.walk("/", max_depth=2)
    client.close_many([1])
    client.close()


def use_native_handles(client: _native.NfsClient) -> None:
    client.close(1)


def invalid_close_shapes(wrapper: SmbClient, native: _native.NfsClient) -> None:
    # Strict mode rejects unused ignores if these unsafe calls become accepted.
    wrapper.close(1)  # type: ignore[call-arg]
    native.close()  # type: ignore[call-arg]
    use_native_handles(wrapper)  # type: ignore[arg-type]
