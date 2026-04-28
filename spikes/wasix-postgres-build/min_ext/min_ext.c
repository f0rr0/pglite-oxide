#include "postgres.h"
#include "fmgr.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(wasix_min_ext_add_one);

Datum
wasix_min_ext_add_one(PG_FUNCTION_ARGS)
{
	int32 value = PG_GETARG_INT32(0);

	PG_RETURN_INT32(value + 1);
}
