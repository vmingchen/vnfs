"""PRR round 3: transactional append must recognize empty files at zero budget."""

import errno
import uuid

import pytest


@pytest.fixture(params=["dummy", "nfs"])
def zero_budget_fs(request):
    fs = request.getfixturevalue(f"{request.param}_fs")
    fs.read_all_max_total_bytes = 0
    return fs


def test_transactional_append_to_empty_file_with_zero_read_budget(zero_budget_fs):
    path = f"/prr-zero-budget-append-{uuid.uuid4().hex}"
    zero_budget_fs.pipe_file(path, b"")

    with zero_budget_fs.transaction:
        with zero_budget_fs.open(path, "ab") as output:
            output.write(b"x")

    zero_budget_fs.read_all_max_total_bytes = 1024
    assert zero_budget_fs.cat_file(path) == b"x"


def test_transactional_append_to_nonempty_file_stays_within_zero_budget(zero_budget_fs):
    path = f"/prr-zero-budget-append-{uuid.uuid4().hex}"
    zero_budget_fs.pipe_file(path, b"existing")

    with pytest.raises(OSError) as error:
        with zero_budget_fs.transaction:
            with zero_budget_fs.open(path, "ab") as output:
                output.write(b"x")
    assert error.value.errno == errno.EFBIG

    zero_budget_fs.read_all_max_total_bytes = 1024
    assert zero_budget_fs.cat_file(path) == b"existing"
    assert not any(".nfs4fs-txn-" in entry for entry in zero_budget_fs.find("/"))
