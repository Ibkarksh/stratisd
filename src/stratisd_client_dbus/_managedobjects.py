# Copyright 2016 Red Hat, Inc.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""
Wrapper for GetManagedObjects() result.
"""

import types

from ._implementation import FilesystemSpec
from ._implementation import ObjectManager
from ._implementation import PoolSpec

_SERVICE_NAME = "org.storage.stratis1"
_POOL_INTERFACE_NAME = "%s.%s" % (_SERVICE_NAME, "pool")
_FILESYSTEM_INTERFACE_NAME = "%s.%s" % (_SERVICE_NAME, "filesystem")

_POOL_INTERFACE_PROPS = frozenset(("Name", "Uuid"))

class ManagedObjects(object):
    """
    Wraps the dict returned by GetManagedObjects() method with some
    methods.
    """
    # pylint: disable=too-few-public-methods


    def __init__(self, objects): # pragma: no cover
        """
        Initializer.

        :param dict objects: the GetManagedObjects result.
        """
        self._objects = objects

    def pools(self, spec=None): # pragma: no cover
        """
        Get the subset of data corresponding to pools and matching spec.

        :param spec: a specification of properties to restrict values returned
        :type spec: dict of str * object
        :returns: a list of pairs of object path/dict for pools only
        :rtype: list of tuple of ObjectPath * dict

        A match requires a conjunction of all specified properties.
        An empty spec results in all pool objects being returned.
        """
        spec = dict() if spec is None else spec
        interface_name = _POOL_INTERFACE_NAME
        return (
           (op, data) for (op, data) in self._objects.items() \
               if interface_name in data.keys() and \
               all(data[interface_name][key] == value \
                   for (key, value) in spec.items())
        )

    def filesystems(self): # pragma: no cover
        """
        Get the subset of data corresponding to filesystems.

        :returns: a list of dictionaries for pools
        :rtype: list of tuple of ObjectPath * dict
        """
        interface_name = _FILESYSTEM_INTERFACE_NAME
        return (
           (x, y) for (x, y) in self._objects.items() \
               if interface_name in y.keys()
        )


def _gmo_builder(spec):
    """
    Returns a function that builds a method interface based on 'spec'.
    This method interface is a simple one to return the values of
    properties from a table generated by a GetManagedObjects() method call.

    :param spec: the interface specification
    :type spec: type, a subtype of InterfaceSpec
    """

    def builder(namespace):
        """
        The property class's namespace.

        :param namespace: the class's namespace
        """

        def build_property(prop): # pragma: no cover
            """
            Build a single property getter for this class.

            :param prop: the property
            """

            def dbus_func(self): # pragma: no cover
                """
                The property getter.
                """
                # pylint: disable=protected-access
                return self._table[spec.INTERFACE_NAME][prop.name]

            return dbus_func

        for prop in spec.PropertyNames:
            namespace[prop.name] = build_property(prop) # pragma: no cover

        def __init__(self, table): # pragma: no cover
            """
            The initalizer for this class.
            """
            self._table = table # pylint: disable=protected-access

        namespace['__init__'] = __init__

    return builder

GMOFilesystem = types.new_class(
   "GMOFilesystem",
   bases=(object,),
   exec_body=_gmo_builder(FilesystemSpec)
)

GMOPool = types.new_class(
   "GMOPool",
   bases=(object,),
   exec_body=_gmo_builder(PoolSpec)
)


def get_managed_objects(proxy): # pragma: no cover
    """
    Convenience function for managed objects.
    :param proxy: proxy for the manager object
    :returns: a constructed ManagedObjects object
    :rtype: ManagedObjects
    """
    return ManagedObjects(ObjectManager.GetManagedObjects(proxy))
