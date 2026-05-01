# Port Forwarding

cella automatically forwards ports from dev containers to your host machine, making services accessible in your browser.

## How It Works

When a service starts listening on a port inside your container, cella detects it and makes it accessible on your host. There are two ways to access forwarded ports:

1. **Hostname URLs** (recommended): `http://3000.main.myapp.localhost`
2. **Port numbers** (traditional): `http://localhost:3000`

Hostname URLs are stable per project/branch identity and each worktree gets its own unique hostname. `localhost:<host_port>` remains the compatibility fallback for every forwarded port.

## Hostname Format

```
http://{port}.{branch}.{project}.localhost
```

- **port**: The container port number (e.g., `3000`)
- **branch**: Your git branch name, sanitized for DNS (e.g., `feature/auth` -> `feature-auth`)
- **project**: Your project name from devcontainer.json `name` field, or the repository directory name

Examples:

```
http://3000.main.myapp.localhost           # main branch, port 3000
http://3000.feature-auth.myapp.localhost   # feature/auth branch, port 3000
http://8080.feature-auth.myapp.localhost   # feature/auth branch, port 8080
http://feature-auth.myapp.localhost        # feature/auth branch, default port
```

If the hostname proxy cannot bind `127.0.0.1:80`, cella binds a high loopback port and shows full URLs:

```
http://3000.feature-auth.myapp.localhost:49180
```

## OrbStack Users

On OrbStack, V1 uses OrbStack's native `.local` domain for the configured default web port only:

```
http://main.myapp.local
http://feature-auth.myapp.local
```

Additional forwarded ports are shown as explicit fallback URLs by `cella ports`. Cella does not claim arbitrary TLDs, LAN exposure, HTTPS/CA trust, command running, or native per-port OrbStack custom domains in V1.

## Viewing Forwarded Ports

```bash
cella ports                # show ports for current container
cella ports --all          # show ports across all worktrees
```

## Configuration

```jsonc
{
  "name": "myapp",                    // Used as {project} in hostnames
  "forwardPorts": [3000, 8080],       // Ports to forward (first = default)
  "portsAttributes": {
    "3000": {
      "label": "Frontend",
      "onAutoForward": "openBrowser",
      "protocol": "http"
    },
    "8080": {
      "label": "API",
      "onAutoForward": "silent"
    }
  }
}
```

## Parallel Development with Worktrees

When using `cella branch create` to work on multiple features, each worktree container gets its own hostname:

```bash
cella branch create feature/auth     # accessible at feature-auth.myapp.localhost
cella branch create feature/billing  # accessible at feature-billing.myapp.localhost
```

Both containers can run the same services on the same ports internally.

## Dev Server Configuration

Some dev servers validate the `Host` header and may reject requests with unfamiliar hostnames. Configure your dev server to accept `*.localhost`:

**Vite:**
```js
// vite.config.js
export default {
  server: { host: true, allowedHosts: true }
}
```

**Next.js:**
```js
// next.config.js
module.exports = { allowedDevHosts: ['*.localhost'] }
```

**Django:**
```python
# settings.py
ALLOWED_HOSTS = ['.localhost']
```

**Rails:**
```ruby
# config/environments/development.rb
config.hosts << /.*\.localhost\z/
```

## Troubleshooting

**"No service found" error page:**
The hostname doesn't match any running container. Run `cella ports --all` to see available services.

**Service returns 403/404 instead of your app:**
Your dev server is likely rejecting the `Host` header. See the dev server configuration section above.

**Port 80 not available:**
Another service is using port 80. The proxy binds a high loopback port instead, and `cella ports` prints hostname URLs with that port suffix. `localhost:<host_port>` URLs still work.

**Safari doesn't resolve `*.localhost`:**
Safari may not resolve deep `*.localhost` subdomains. Use Chrome or Firefox, or add specific entries to `/etc/hosts`:
```
127.0.0.1  3000.main.myapp.localhost
127.0.0.1  3000.feature-auth.myapp.localhost
```
