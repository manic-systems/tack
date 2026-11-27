# SPDX-License-Identifier: EUPL-1.2

# stands in for a resolver from before __tack_nodes
removeAttrs (import ../../../../.tack { resolverDir = ./.; }) [ "__tack_nodes" ]
