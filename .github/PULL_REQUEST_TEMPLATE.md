## Summary

Describe the user-visible outcome and why this change is needed.

## Checklist

- [ ] The reasoning is in the commit body, not only here.
- [ ] A test at the behavior seam this changes.
- [ ] A break says so with `!` in its subject. The MCP tool surface is a
      public contract: tool names, parameter names, and the shape of a result.
- [ ] Anything that can only be proven on a desktop was run against a real
      Hyprland, and the message says what was observed.

[CONTRIBUTING.md](https://github.com/thelipe7/computer-use-hyprland/blob/main/CONTRIBUTING.md)
says what each of those means and why it is the one asked for.

<!--
Deliberately short. CI already checks the formatting, the lints, the spelling,
the dependency policy, the workflows, this title, every commit subject and
sign-off, and the tool contract.

What is left is the judgment: whether a break is a break, and whether anything
that needs a running compositor was actually run against one.
-->
