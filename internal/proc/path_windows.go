package proc

// UserPath has nothing to do on Windows: an app started from the Start menu
// gets the user's PATH from the registry, as a terminal does.
func UserPath() {}
