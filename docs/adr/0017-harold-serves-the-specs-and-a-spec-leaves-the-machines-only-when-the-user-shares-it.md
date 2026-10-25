# 0017. Harold serves the specs, and a spec leaves the machines only when the user shares it

Date: 2026-10-25
Status: Accepted (serve-specs). Builds on 0002, 0007, 0012 and 0015.

## Context

The chief published every spec as a private artifact, so each spec left the machines whether or not the user wanted to share it. The user reads a spec on the phone, over Tailscale, and at the desk, in a browser on the Mac. The specs are files in each product's `.superpowers/pod/specs/` folder.

## Decision

- Harold renders each product's specs as pages: `GET /specs` and `GET /specs/{product}/{id}`. It finds the specs through the `Products` port, as the question tool does ([ADR 0015](0015-the-question-tool-reads-the-spec-files-the-chief-names.md)). It resolves symlinks, refuses a file outside the specs folder or over 1 MiB, and shows no path. Raw HTML in a spec shows as text, and a link keeps its `href` only for `http:`, `https:` or a relative address.
- The web app serves the pages behind the login, pairing and same-origin checks ([ADR 0007](0007-the-web-app-reaches-todos-through-the-question-service.md)). A second listener, `[web_app] specs_listen` (`127.0.0.1:4331` by default), serves the same two routes and three static files to the browser on the Mac. It is open only when `[telemetry] pod_registry` is set. It has no login or pairing check, because a browser on the Mac sends no Tailscale login. It refuses a `Host` that is not its own loopback name, which stops DNS rebinding, and it answers no method but `GET` and `HEAD`.
- A todo's link comes from its `spec_file`: the question tool gives `spec_page` for each todo and `local_specs_url` for the listing. The web app card opens the page on the web app, and the dash `o` opens it on the specs listener.
- A spec leaves the machines only when the user presses **Share** on a spec page in the web app. The button posts `/specs/{product}/{id}/share`, behind the same checks. Harold puts the fixed reply `Share spec <product>/<id> sha <12>` in the thread, and the reply goes to the chief by the existing path. The chief has the PnEM publish that one spec as a private artifact. The specs listener has no **Share** button and answers no `POST`.
- Mermaid draws the diagrams in the page. It is one vendored bundle, built from a pinned, audited npm lockfile ([ADR 0012](0012-browser-libraries-are-vendored-as-one-bundle-built-from-a-pinned-audited-npm-lockfile.md)), and it runs with `securityLevel: 'strict'`. A spec page carries its own content security policy, with `style-src 'self' 'unsafe-inline'`, because Mermaid writes a `<style>` into each diagram. Scripts stay `'self'`.

## Consequences

- Any local user or process can read every spec through the specs listener, as it can read the files. [ADR 0002](0002-tool-services-trust-local-callers.md) trusts local callers, and the listener answers no write, so no local process can make the chief publish a spec through it.
- The spec pages hold no more than the spec files hold. The web app shows them to the one paired device on the tailnet.
- The chief's and the PnEM's change, to publish one spec only on request, is made outside Harold after this merge.
- The phone downloads the Mermaid bundle, about 5 MB, once for each bundle name, because the versioned file is served as immutable.
- The Mermaid bundle leaves out `elkjs` (EPL-2.0). A diagram that asks for the ELK layout shows its source.
- Upgrading Mermaid is a deliberate, audited act, as for the OpenTelemetry bundle.
