"""Tests for RealmSubgraph realm isolation."""

import pytest

from .realms import RealmSubgraph


class MockBaseGraph:
    """A minimal mock of BaseGraph for testing."""

    def __init__(self) -> None:
        self._nodes: dict = {}
        self._edges: list = []

    def add_node(self, node_id: str, data: dict | None = None) -> None:
        if data is None:
            data = {}
        self._nodes[node_id] = {"id": node_id, "data": data, "realm": "other"}

    def get_nodes(self, realm: str | None = None) -> dict:
        if realm is None:
            return dict(self._nodes)
        return {
            k: v for k, v in self._nodes.items()
            if v.get("realm") == realm
        }

    def get_edges(self, realm: str | None = None) -> list:
        if realm is None:
            return list(self._edges)
        return [
            e for e in self._edges
            if e[2].get("realm") == realm
        ]

    def has_node(self, node_id: str, realm: str | None = None) -> bool:
        if realm is None:
            return node_id in self._nodes
        node = self._nodes.get(node_id)
        return node is not None and node.get("realm") == realm

    def has_edge(self, source: str, target: str, realm: str | None = None) -> bool:
        if realm is None:
            return (source, target) in [(e[0], e[1]) for e in self._edges]
        return any(
            e[0] == source and e[1] == target and e[2].get("realm") == realm
            for e in self._edges
        )


class TestRealmSubgraphInitialization:
    """Test RealmSubgraph initialization."""

    def test_init_with_realm_id(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        assert realm_sub.realm_id == "realm-1"

    def test_init_default_isolation(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        assert realm_sub.allow_cross_realm_reads is False

    def test_init_scoped(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        assert realm_sub.is_scoped() is True

    def test_init_with_cross_realm_allowed(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1", allow_cross_realm_reads=True)
        assert realm_sub.allow_cross_realm_reads is True


class TestRealmSubgraphIsolation:
    """Test realm isolation boundaries."""

    def test_reject_cross_realm_read_on_get_nodes(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        with pytest.raises(ValueError, match="Cross-realm read denied"):
            realm_sub.get_nodes(realm="realm-2")

    def test_reject_cross_realm_read_on_get_edges(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        with pytest.raises(ValueError, match="Cross-realm read denied"):
            realm_sub.get_edges(realm="realm-2")

    def test_reject_cross_realm_read_on_has_node(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        with pytest.raises(ValueError, match="Cross-realm read denied"):
            realm_sub.has_node("node-1", realm="realm-2")

    def test_reject_cross_realm_read_on_has_edge(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        with pytest.raises(ValueError, match="Cross-realm read denied"):
            realm_sub.has_edge("a", "b", realm="realm-2")

    def test_allow_same_realm_reads(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        realm_sub.add_node("node-1", {"name": "test"})
        assert realm_sub.get_nodes() == {"node-1": {"id": "node-1", "data": {"name": "test"}, "realm": "realm-1"}}

    def test_allow_cross_realm_with_flag(self) -> None:
        base = MockBaseGraph()
        base.add_node("remote-node", {"source": "remote"})
        realm_sub = RealmSubgraph(base, realm_id="realm-1", allow_cross_realm_reads=True)
        nodes = realm_sub.get_nodes(realm="other")
        assert "remote-node" in nodes


class TestRealmSubgraphOperations:
    """Test RealmSubgraph add/query operations."""

    def test_add_node(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        realm_sub.add_node("node-1", {"name": "test"})
        assert "node-1" in realm_sub.get_nodes()

    def test_add_edge(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-1")
        realm_sub.add_node("node-1")
        realm_sub.add_node("node-2")
        realm_sub.add_edge("node-1", "node-2")
        edges = realm_sub.get_edges()
        assert len(edges) == 1
        assert edges[0][0] == "node-1"
        assert edges[0][1] == "node-2"

    def test_get_realm_id(self) -> None:
        base = MockBaseGraph()
        realm_sub = RealmSubgraph(base, realm_id="realm-42")
        assert realm_sub.get_realm_id() == "realm-42"
