def greet(name: str) -> str:
    message = f"hellow, {name}"
    return message




class User:
    def __init__(self, username: str) -> None:
        self.username = username

    def display_name(self) -> str:
        return self.username.title()


class Account:
    def __init__(self, id: int):
        self.id = id



user = User("narendra")
result = greet(user.display_name())
print(result)
print(user)