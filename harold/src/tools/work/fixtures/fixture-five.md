# fixture-five: Alert when a rotation fails

Priority: later
Status: released
Waits for: the user's check of the alert in HyperDX
Next step: Ask the user to check the alert.

Decisions:
- agreed, the user, 2026-10-20: The alert fires once for each failure, not for each retry.
- dropped, the chief, 2026-10-21: An alert for each retry.
- agreed, the chief, 2026-10-22: Severity is warn.

Links:
- spec:harold/work-alerts
- question:q-0397
- note:Expected count is zero.
