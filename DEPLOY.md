# Deployment workflow

How a change gets from a source tree to a running sandbox.

## The short version

```
edit -> CI -> tag -> release asset -> image build (CI) -> kubectl set image -> done
```

**The cluster pulls images directly.** There is no mirror step, no
copy-to-an-internal-registry step, and no push to a second registry. A
deployment names the upstream image and Kubernetes fetches it:

```yaml
image: ghcr.io/easydev-org/sandbox-windows:latest
imagePullPolicy: Always
```

### Why this is the rule now

The cluster could not reach `registry-1.docker.io` for a while, and every new pod
failed at `FailedCreatePodSandBox` while pulling `rancher/mirrored-pause:3.6` -
the sandbox image, which is pulled before the pod image is even considered. The
workaround was to copy each image into the internal artifact registry by hand,
which is why the old deployments named `artifact.worker.svc.cluster.local/...`.

That network fault is fixed. Two checks confirmed it:

```
docker.io/library/alpine:3.20                            -> Running
ghcr.io/easydev-org/sandbox-wayland:latest                -> Running
```

So the mirror step is gone. If pods start failing at `FailedCreatePodSandBox`
again, the cluster has lost its route to the registry - that is an
infrastructure problem, not an image problem, and it affects a 200 MB image and
an 18 GB image identically.

### What this costs

Pulling 18 GB over the public internet takes tens of minutes the first time.
The image is then cached on the node, so a redeploy of the same digest is fast.
`imagePullPolicy: Always` forces a registry round-trip to compare digests but
still reuses the layers.

## The chain, step by step

### 1. Source -> release asset

Each platform is its own repository, and each one's `release.yml` runs on a tag:

| Repository | Runner | Asset |
|---|---|---|
| `easystream` (Linux) | ubuntu-latest, arm64 | `easystream-linux-{amd64,arm64}` |
| `easystream-windows` | **windows-latest** (native) | `easystream-windows-{amd64,arm64}.exe` |
| `easystream-macos` | **macos-15** (native) | `easystream-macos-{amd64,arm64}` |
| `easystream-android` | ubuntu + gradle | `easystream-android.apk` |

Native runners are not an optimisation. Windows cross-compiled from Linux could
not run its own tests and had to disable FORTIFY to satisfy mingw; macOS needs
Xcode. A tag is the only thing that produces an asset.

### 2. Release asset -> image

`easysandbox-{windows,macos,android}` and `easysandbox` (the Linux desktops) each
have an `image` workflow that resolves the **latest** release of the daemon and
downloads it at build time. So the daemon version in an image is whatever was
latest when the image was built - not what is latest now.

**Order matters**: tag the daemon first, wait for its release to publish, then
dispatch the image build. Doing it the other way bakes the previous version in,
and the only symptom is an image that is a few KB too small.

### 3. Image -> cluster

```bash
kubectl set image deploy/<name> -n worker <container>=ghcr.io/easydev-org/<image>:latest
```

No mirror. The cluster pulls it.

## Verifying a deployment actually took

An image that is rebuilt but whose daemon is old is the failure mode this project
has hit most often, and it is invisible: the pod is Running, the log looks
healthy, and the feature is simply absent.

**Check the daemon, not the pod.** Every daemon reports its own version through
its binary on disk:

```bash
kubectl exec -n worker deploy/<name> -c <container> -- ls -la /opt/easyworker/easystream
```

Compare the size against the release asset. A mismatch means the image was built
before the release published, or a `COPY` layer was cached.

Then check the wire, not the log: a Hello from the daemon is 23 bytes and carries
the video block. `e2e/configset.py` and `e2e/pli.py` assert exactly that.

## Traps

### BuildKit caches COPY on the file NAME

```dockerfile
COPY ./easyworker /usr/local/bin/easyworker     # cached forever
```

A new release downloaded to the same path is ignored: the image rebuilds, its
digest changes (other layers differ), and the binary inside is weeks old. Every
Containerfile now takes `ARG BIN_HASH` before those COPYs and CI passes a hash of
the downloaded binaries. The same class of bug hit `COPY oem.sh`.

### `imagePullPolicy: IfNotPresent` plus a floating tag

`kubectl rollout restart` does not re-pull. Use `Always`.

### The old hand-mirror script is not needed

`scripts/mirror_image.py` still exists and still works (it learned to resume a
stalled blob upload and to verify the digest landed), but nothing calls it. Do not
reach for it when a pull is slow: a slow pull is 18 GB over the public internet,
not a broken registry.
