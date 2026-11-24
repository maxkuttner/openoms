# docker

The container image's start script: `entrypoint.sh` sets up the database on first start, then serves; `entrypoint_test.sh` tests it (run in CI). The image itself is built from the root `Dockerfile`.
