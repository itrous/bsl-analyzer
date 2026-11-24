&После("ПередЗаписью")
Процедура Первый()
КонецПроцедуры

&After("ПЕРЕДЗАПИСЬЮ")
Procedure Second()
EndProcedure

&Before("BeforeWrite")
Procedure BeforeOne()
EndProcedure

&Перед("beforewrite")
Процедура BeforeTwo()
КонецПроцедуры

&Вместо("BeforeWrite")
Процедура Around()
КонецПроцедуры

&После("OtherTarget")
Процедура Other()
КонецПроцедуры
