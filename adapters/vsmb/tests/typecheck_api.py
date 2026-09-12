"""Static public-API smoke test; mypy checks this file in CI."""

from vsmb import SmbClient


def use_api(client: SmbClient) -> None:
    stats, errors = client.stat_many(["/a", "/b"])
    assert len(stats) == 2
    assert isinstance(errors, dict)
    data, read_errors = client.read_many(["/a"], [0], [4])
    assert data[0] in (None, b"data")
    assert isinstance(read_errors, dict)
    client.write_many(["/out"], [b"data"])
    client.close()
