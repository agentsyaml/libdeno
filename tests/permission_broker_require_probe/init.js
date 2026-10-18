import { op_require_read_package_scope } from "ext:core/ops";
import { op_require_package_imports_resolve } from "ext:core/ops";
import { core } from "ext:core/mod.js";

globalThis.__brokerPackageScope = op_require_read_package_scope;
globalThis.__brokerPackageImports = op_require_package_imports_resolve;
globalThis.__installPermissionBroker = core.ops.op_install_permission_broker;
