# Lessons

- When recognizing external agent or tool processes inside tmux panes, prefer matching configured fragments against the pane process and descendant process commands. Avoid relying only on `pane_current_command`, because wrappers can expose misleading names like Node versions or architecture-specific launcher names.
- Do not turn a reversible retention-policy choice into an upfront architecture gate. Separate durable facts from transient work state, preserve both options in the design, and defer deletion timing until there is an operational need or measured storage evidence.
- For a daemon change, source tests and review are not the delivery boundary. If the request is to make the running service work, completion requires building the deployable artifact, installing it through the documented workflow, restarting the daemon, and exercising the live ingress-to-handler path.
