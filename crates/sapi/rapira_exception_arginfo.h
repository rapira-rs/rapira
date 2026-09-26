/* This is a generated file, edit the .stub.php file instead.
 * Stub hash: f180c212bfcae309577eb6639e8dc561bfcfa9ac */

static zend_class_entry *register_class_Rapira_Exception_RapiraThrowable(zend_class_entry *class_entry_Throwable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Exception", "RapiraThrowable", NULL);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Throwable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Exception_ClosedException(zend_class_entry *class_entry_RuntimeException, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Exception", "ClosedException", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_RuntimeException, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Exception_TimeoutException(zend_class_entry *class_entry_RuntimeException, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Exception", "TimeoutException", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_RuntimeException, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Exception_WorkDiscardedException(zend_class_entry *class_entry_RuntimeException, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Exception", "WorkDiscardedException", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_RuntimeException, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Exception_NoDispatcherError(zend_class_entry *class_entry_Error, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Exception", "NoDispatcherError", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_Error, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Exception_NotInWorkerModeError(zend_class_entry *class_entry_Error, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Exception", "NotInWorkerModeError", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_Error, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Exception_AlreadyFinalizedError(zend_class_entry *class_entry_Error, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Exception", "AlreadyFinalizedError", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_Error, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}
