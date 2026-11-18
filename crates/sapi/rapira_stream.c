#include "rapira_sapi.h"

#include <Zend/zend_generators.h>

// rust glue: Held::send of the unit (work.rs). False: the host closed the unit.
extern bool rapira_rs_stream_send(const rapira_held *held, const char *message,
                                  size_t len);

// Userland runs only below this C frame, so a bailout from the generator
// crosses no Rust frame.
rapira_loop rapira_stream_run_generator(zend_object *generator,
                                        const rapira_held *held) {
    zend_generator *gen = (zend_generator *)generator;
    bool closed = false;

    // A generator that has not run goes to its first yield, as
    // zend_generator_ensure_initialized does:
    // https://github.com/php/php-src/blob/php-8.5.12/Zend/zend_generators.c#L908-L914
    if (Z_TYPE(gen->value) == IS_UNDEF && gen->execute_data &&
        gen->node.parent == NULL) {
        zend_generator_resume(gen);
        gen->flags |= ZEND_GENERATOR_AT_FIRST_YIELD;
    }
    while (!EG(exception) && gen->execute_data) {
        if (!closed) {
            // During a yield from, the current generator holds the value. A
            // generator that yields by reference holds a reference.
            zval *value = &zend_generator_get_current(gen)->value;
            ZVAL_DEREF(value);
            if (Z_TYPE_P(value) != IS_STRING) {
                zend_argument_type_error(1, "must yield strings, %s yielded",
                                         zend_zval_value_name(value));
                return RAPIRA_LOOP_THROWN;
            }
            if (rapira_rs_stream_send(held, Z_STRVAL_P(value),
                                      Z_STRLEN_P(value))) {
                zend_generator_resume(gen);
                continue;
            }
            closed = true;
        }
        // Each yield after the close gets a new exception, built as
        // zend_throw_exception builds one. Generator::throw resumes the
        // generator with it.
        zval discarded;
        object_init_ex(&discarded, rapira_ce_work_discarded_exception);
        zend_update_property_string(rapira_ce_work_discarded_exception,
                                    Z_OBJ(discarded), ZEND_STRL("message"),
                                    "the host closed the work first");
        zend_call_method_with_1_params(generator, zend_ce_generator, NULL,
                                       "throw", NULL, &discarded);
        zval_ptr_dtor(&discarded);
    }
    if (EG(exception)) {
        return RAPIRA_LOOP_THROWN;
    }
    return closed ? RAPIRA_LOOP_CLOSED : RAPIRA_LOOP_DONE;
}
