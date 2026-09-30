// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The sync-suite guest module.
//
// Every echo handler forwards the value to the host import of the same name, so
// a marshalling bug in either direction (lift on the import, lower on the
// import return, lower on the export return) fails the round-trip. Exports are
// provided in the package layer, as members of `testSyncSuite`.

import * as echoes from "test:sync-suite/echoes";
import * as hostThingInterface from "test:sync-suite/host-thing-interface";

const echoesImpl = {
  echoNothing: function () {
    return echoes.echoNothing();
  },
  echoBool: function (v) {
    return echoes.echoBool(v);
  },
  echoU8: function (v) {
    return echoes.echoU8(v);
  },
  echoS8: function (v) {
    return echoes.echoS8(v);
  },
  echoU16: function (v) {
    return echoes.echoU16(v);
  },
  echoS16: function (v) {
    return echoes.echoS16(v);
  },
  echoU32: function (v) {
    return echoes.echoU32(v);
  },
  echoS32: function (v) {
    return echoes.echoS32(v);
  },
  echoU64: function (v) {
    return echoes.echoU64(v);
  },
  echoS64: function (v) {
    return echoes.echoS64(v);
  },
  echoChar: function (v) {
    return echoes.echoChar(v);
  },
  echoF32: function (v) {
    return echoes.echoF32(v);
  },
  echoF64: function (v) {
    return echoes.echoF64(v);
  },
  echoString: function (v) {
    return echoes.echoString(v);
  },
  echoListBool: function (v) {
    return echoes.echoListBool(v);
  },
  echoListU8: function (v) {
    return echoes.echoListU8(v);
  },
  echoListListU8: function (v) {
    return echoes.echoListListU8(v);
  },
  echoListListListU8: function (v) {
    return echoes.echoListListListU8(v);
  },
  echoOptionU8: function (v) {
    return echoes.echoOptionU8(v);
  },
  echoOptionOptionU8: function (v) {
    return echoes.echoOptionOptionU8(v);
  },
  echoResultU8U8: function (v) {
    return echoes.echoResultU8U8(v);
  },
  echoResultResultU8U8U8: function (v) {
    return echoes.echoResultResultU8U8U8(v);
  },
  echoListS8: function (v) {
    return echoes.echoListS8(v);
  },
  echoListU16: function (v) {
    return echoes.echoListU16(v);
  },
  echoListS16: function (v) {
    return echoes.echoListS16(v);
  },
  echoListU32: function (v) {
    return echoes.echoListU32(v);
  },
  echoListS32: function (v) {
    return echoes.echoListS32(v);
  },
  echoListU64: function (v) {
    return echoes.echoListU64(v);
  },
  echoListS64: function (v) {
    return echoes.echoListS64(v);
  },
  echoListChar: function (v) {
    return echoes.echoListChar(v);
  },
  echoListF32: function (v) {
    return echoes.echoListF32(v);
  },
  echoListF64: function (v) {
    return echoes.echoListF64(v);
  },
  echoListString: function (v) {
    return echoes.echoListString(v);
  },
  echoMany: function (v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15, v16) {
    return echoes.echoMany(
      v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15, v16,
    );
  },
  echoResource: function (v) {
    return echoes.echoResource(v);
  },
  acceptBorrow: function (v) {
    return echoes.acceptBorrow(v);
  },
  echoRecord: function (v) {
    return echoes.echoRecord(v);
  },
  echoEnum: function (v) {
    return echoes.echoEnum(v);
  },
  echoVariant: function (v) {
    return echoes.echoVariant(v);
  },
};

const hostThingDriverImpl = {
  drive: function (s) {
    const thing = new hostThingInterface.HostThing(s);
    const a = thing.get();
    const b = hostThingInterface.HostThing.getStatic(thing);
    return `${a}|${b}`;
  },
};

// The package layer of the export names: `test:sync-suite`'s interfaces
// are members of `testSyncSuite`.
export const testSyncSuite = {
  echoes: echoesImpl,
  hostThingDriver: hostThingDriverImpl,
};
