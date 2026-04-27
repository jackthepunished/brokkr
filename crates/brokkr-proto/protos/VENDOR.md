# Vendored protobuf definitions

These files are copied verbatim from upstream repositories so the Brokkr
build does not depend on network access. **Do not edit them.** To refresh,
re-run the steps below at a later upstream commit.

## Sources

| Path                                | Upstream                                                                                  |
|-------------------------------------|-------------------------------------------------------------------------------------------|
| `build/bazel/remote/execution/v2/`  | [bazelbuild/remote-apis][rapis] @ `becdd8f9ff811df88a22d3eadd6341753d51d167`              |
| `build/bazel/semver/`                | [bazelbuild/remote-apis][rapis] @ `becdd8f9ff811df88a22d3eadd6341753d51d167`              |
| `google/api/`                        | [googleapis/googleapis][gapis] @ `a7f4ee2ba387f74da14ca80cf31f43836b3272ea`               |
| `google/bytestream/`                 | [googleapis/googleapis][gapis] @ `a7f4ee2ba387f74da14ca80cf31f43836b3272ea`               |
| `google/longrunning/`                | [googleapis/googleapis][gapis] @ `a7f4ee2ba387f74da14ca80cf31f43836b3272ea`               |
| `google/rpc/`                        | [googleapis/googleapis][gapis] @ `a7f4ee2ba387f74da14ca80cf31f43836b3272ea`               |

[rapis]: https://github.com/bazelbuild/remote-apis
[gapis]: https://github.com/googleapis/googleapis

## Refresh procedure

```sh
git clone --depth 1 https://github.com/bazelbuild/remote-apis /tmp/remote-apis
git clone --depth 1 https://github.com/googleapis/googleapis    /tmp/googleapis

DST=crates/brokkr-proto/protos
cp /tmp/remote-apis/build/bazel/remote/execution/v2/remote_execution.proto \
   $DST/build/bazel/remote/execution/v2/
cp /tmp/remote-apis/build/bazel/semver/semver.proto $DST/build/bazel/semver/
cp /tmp/googleapis/google/api/{annotations,http,client,field_behavior}.proto $DST/google/api/
cp /tmp/googleapis/google/bytestream/bytestream.proto $DST/google/bytestream/
cp /tmp/googleapis/google/longrunning/operations.proto $DST/google/longrunning/
cp /tmp/googleapis/google/rpc/status.proto $DST/google/rpc/
```

Then bump the SHAs above and run `cargo build -p brokkr-proto` to regenerate.
