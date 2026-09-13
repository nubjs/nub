# Restricted LowBox filesystem probe

This standalone Windows fixture compares ordinary and uniquely restricted primary tokens before and after LowBox conversion. It uses both `CreateAppContainerToken` and `NtCreateLowBoxToken`. No adapter, broker, permissive startup impersonation, installed-image modification, or system ACL change is used.

The runner prepares a disposable standard account. That account creates its own AppContainer profile and controlled files, grants only fixture objects, and launches an independently built child. The profile and account are removed after evidence collection.

Each token records its groups, restricting SIDs, privileges, package identity, integrity level, and LPAC query result. Each file has ordinary-user authority plus the labelled optional AAP, ARAP, capability, package, or restricting-SID grant. A null-DACL file and hardlink aliases are controls. The fixture records `AccessCheck`, native opens under same-thread impersonation, and actual child startup separately. An inherited write-only output handle carries child readiness; it is not a fresh filesystem grant.

The workflow is branch-scoped and builds only these C++ fixtures. A successful workflow means the experiment completed and its ordinary child control ran; token conversion or restricted-child startup failures remain findings in the logs, not a passing confinement verdict. LPAC is observed but not requested in this first discriminator.
