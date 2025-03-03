import sys


PY2 = sys.version_info[0] == 2

if PY2:
    text_type = unicode  # noqa
    int_types = (int, long)  # noqa
    string_types = (str, unicode)  # noqa
    range_type = xrange  # noqa
    iteritems = lambda x: x.iteritems()
    itervalues = lambda x: x.itervalues()
    NUL = "\x00"

    def implements_to_string(cls):
        cls.__unicode__ = cls.__str__
        cls.__str__ = lambda x: x.__unicode__().encode("utf-8")
        return cls

else:
    text_type = str
    int_types = (int,)
    string_types = (str,)
    range_type = range
    iteritems = lambda x: x.items()
    itervalues = lambda x: x.values()
    NUL = 0
    implements_to_string = lambda x: x


def with_metaclass(meta, *bases):
    class metaclass(type):
        def __new__(cls, name, this_bases, d):
            return meta(name, bases, d)

    return type.__new__(metaclass, "temporary_class", (), {})
