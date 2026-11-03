# Security Policy

ROS is a hobby and teaching operating system. It runs in a virtual machine, has
no user accounts, no network stack and no memory protection between
components. It is **not** meant to protect anything, so please don't use it
where security matters.

That said, we're happy to hear about problems such as:

- memory-safety bugs (undefined behavior) in the Rust code,
- ways malformed firmware data (ACPI tables, USB descriptors) can crash or
  corrupt the kernel,
- issues in the build scripts or CI workflows (supply chain).

## Supported versions

Only the latest release and the `main` branch get fixes.

## Reporting a vulnerability

Please report it privately through
[GitHub private vulnerability reporting](https://github.com/rene-schakmann/ros/security/advisories/new)
rather than in a public issue. Include steps to reproduce and the serial log if
possible. You should get a response within a week.
