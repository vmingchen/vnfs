# Patched NFS-Ganesha CI image

This image pins the patched `vfsi/nfs-ganesha` source used to validate
successful NFSv4.2 server-side `COPY`. It intentionally builds only the
NFSv4 server and VFS FSAL needed by the integration suite.

FSAL_VFS uses Linux persistent file handles, so the container must run with
the privileges required by `open_by_handle_at(2)`. The CI job uses an
ephemeral privileged container and bind-mounts its test directory at
`/export`.

Update `GANESHA_REVISION` deliberately. Do not track a moving branch: server
behavior is part of the integration-test fixture.
