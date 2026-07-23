class FabricPair:
    __slots__ = ('node_a', 'node_b', 'subnet')

    def __init__(self, node_a, node_b, subnet):
        self.node_a = node_a
        self.node_b = node_b
        self.subnet = subnet

    def __repr__(self):
        return f"FabricPair(node_a={self.node_a}, node_b={self.node_b}, subnet={self.subnet})"

    def to_db(self):
        return {
            'node_a': self.node_a,
            'node_b': self.node_b,
            'subnet': self.subnet
        }

    @classmethod
    def from_db(cls, data):
        return cls(data['node_a'], data['node_b'], data['subnet'])
