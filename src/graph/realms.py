"""Realm-scoped graph subgraph utilities for ForgeFleet."""

from __future__ import annotations

from typing import Any, Dict, List, Optional, Set, Tuple

from .base import BaseGraph


class RealmSubgraph:
    """A realm-scoped view of a base graph container.

    Enforces realm isolation by providing scoped reads and writes
    while preventing cross-realm access by default.
    """

    def __init__(
        self,
        base: BaseGraph,
        realm_id: str,
        allow_cross_realm_reads: bool = False,
    ) -> None:
        """Initialize a new realm-scoped subgraph.

        Args:
            base: The underlying base graph container.
            realm_id: The unique identifier for this realm.
            allow_cross_realm_reads: If True, allow reading data from
                other realms. Defaults to False for strict isolation.
        """
        self.base = base
        self.realm_id = realm_id
        self.allow_cross_realm_reads = allow_cross_realm_reads

        # Initialize an isolated namespace for this realm
        self._namespace: Dict[str, Any] = {}
        self._nodes: Dict[str, Dict[str, Any]] = {}
        self._edges: List[Tuple[str, str, Dict[str, Any]]] = []

    def get_nodes(self, realm: Optional[str] = None) -> Dict[str, Dict[str, Any]]:
        """Get all nodes scoped to the current realm.

        Args:
            realm: Optional realm override. If not provided, uses
                the subgraph's own realm_id.

        Returns:
            A dictionary of nodes belonging to the specified realm.

        Raises:
            ValueError: If a different realm is requested and
                cross-realm reads are not allowed.
        """
        target_realm = realm if realm is not None else self.realm_id

        if target_realm != self.realm_id and not self.allow_cross_realm_reads:
            raise ValueError(
                f"Cross-realm read denied: current realm is '{self.realm_id}', "
                f"requested realm is '{target_realm}'"
            )

        if target_realm == self.realm_id:
            return dict(self._nodes)
        else:
            # Cross-realm read from base graph
            return self.base.get_nodes(realm=target_realm)

    def get_edges(self, realm: Optional[str] = None) -> List[Tuple[str, str, Dict[str, Any]]]:
        """Get all edges scoped to the current realm.

        Args:
            realm: Optional realm override. If not provided, uses
                the subgraph's own realm_id.

        Returns:
            A list of edges belonging to the specified realm.

        Raises:
            ValueError: If a different realm is requested and
                cross-realm reads are not allowed.
        """
        target_realm = realm if realm is not None else self.realm_id

        if target_realm != self.realm_id and not self.allow_cross_realm_reads:
            raise ValueError(
                f"Cross-realm read denied: current realm is '{self.realm_id}', "
                f"requested realm is '{target_realm}'"
            )

        if target_realm == self.realm_id:
            return list(self._edges)
        else:
            # Cross-realm read from base graph
            return self.base.get_edges(realm=target_realm)

    def is_scoped(self) -> bool:
        """Check whether this subgraph is scoped to a single realm.

        Returns:
            True if the subgraph enforces realm isolation, False otherwise.
        """
        return True

    def add_node(
        self,
        node_id: str,
        data: Optional[Dict[str, Any]] = None,
    ) -> None:
        """Add a node to the current realm.

        Args:
            node_id: The unique identifier for the node.
            data: Optional node attributes/data.
        """
        if data is None:
            data = {}
        self._nodes[node_id] = {
            "id": node_id,
            "data": data,
            "realm": self.realm_id,
        }

    def add_edge(
        self,
        source: str,
        target: str,
        data: Optional[Dict[str, Any]] = None,
    ) -> None:
        """Add an edge to the current realm.

        Args:
            source: The source node ID.
            target: The target node ID.
            data: Optional edge attributes/data.
        """
        if data is None:
            data = {}
        self._edges.append((source, target, {"data": data, "realm": self.realm_id}))

    def get_realm_id(self) -> str:
        """Get the realm ID for this subgraph.

        Returns:
            The realm ID string.
        """
        return self.realm_id

    def has_node(self, node_id: str, realm: Optional[str] = None) -> bool:
        """Check if a node exists in the specified realm.

        Args:
            node_id: The node ID to check.
            realm: Optional realm override.

        Returns:
            True if the node exists in the specified realm.
        """
        target_realm = realm if realm is not None else self.realm_id

        if target_realm != self.realm_id and not self.allow_cross_realm_reads:
            raise ValueError(
                f"Cross-realm read denied: current realm is '{self.realm_id}', "
                f"requested realm is '{target_realm}'"
            )

        if target_realm == self.realm_id:
            return node_id in self._nodes
        else:
            return self.base.has_node(node_id, realm=target_realm)

    def has_edge(self, source: str, target: str, realm: Optional[str] = None) -> bool:
        """Check if an edge exists in the specified realm.

        Args:
            source: The source node ID.
            target: The target node ID.
            realm: Optional realm override.

        Returns:
            True if the edge exists in the specified realm.
        """
        target_realm = realm if realm is not None else self.realm_id

        if target_realm != self.realm_id and not self.allow_cross_realm_reads:
            raise ValueError(
                f"Cross-realm read denied: current realm is '{self.realm_id}', "
                f"requested realm is '{target_realm}'"
            )

        if target_realm == self.realm_id:
            return (source, target) in [
                (e[0], e[1]) for e in self._edges
            ]
        else:
            return self.base.has_edge(source, target, realm=target_realm)
